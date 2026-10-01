//! Bun agent event loop.
//!
//! Ties the supervisor, health checker, and container runtime together
//! into a single async event loop. Commands arrive over an `mpsc` channel;
//! health checks fire on a timer; shutdown is coordinated via a
//! `CancellationToken`.
//!
//! # The loop rule
//!
//! `run_loop` is the only owner of a node's state, so it runs one turn at a
//! time, and every caller waits for the turn in progress. A turn must be
//! short: the turn budget is 1 s ([`super::loop_meter::TURN_BUDGET`]), and
//! the V02 soak fails a tier on any turn over it. So:
//!
//! 1. An `await` that a turn reaches must not wait on anything slow: a
//!    runtime call, a council write, a subprocess, the network, a peer, the
//!    disk beyond one persist, or a client draining a channel. Move the work
//!    off the loop in one of the ways the agent already does:
//!    - **Answer from a task** when nothing on the loop needs the result
//!      (`logs`, `council_requests`, a node-kill's container kills).
//!    - **Let the task report back** through a `select!` branch when the
//!      loop has to finish the job: stop waits, identity signings, restart
//!      steps, state sweeps, and the [`follow_ups`] enum (upgrades, `nft`,
//!      egress DNS, node pressure).
//!    - **Have whoever drove the runtime read it**: a deploy worker or a
//!      restart step hands over the network reference it retained and the
//!      [`launch_evidence`] of what it started, with the step that records it.
//!    - **Start it, and collect it on a later turn** ([`off_loop_work`]):
//!      a step whose disk or runtime work isn't done within the turn fails
//!      with [`BunError::StillRunning`], and its caller asks again.
//!    - **Bound it by the turn's runtime budget**: reads a later turn can
//!      retry wait at most until [`BunAgent::turn_deadline`], which every
//!      such await in a turn shares (`tokio::time::timeout_at`).
//! 2. An await that stays inline without a deadline carries a
//!    `// LOOP-INLINE: <why>` comment on its statement: an in-memory lock,
//!    an fsync'd persist (allowed, and bounded by the harness's slow-disk
//!    scenario), or the short sleep before an upgrade's exec.
//! 3. `loop_rule::every_inline_await_on_the_agent_loop_has_a_deadline_or_a_reason`
//!    walks every method a turn can reach and fails on an await with
//!    neither. Reviewers read the tags, not the whole call graph.
//! 4. The starvation harness (`tests::loop_harness`) proves it: each
//!    scenario makes one await slow and checks that a queued status is
//!    answered, and the worst turn ends, within the budget.
//!
//! Every turn is timed by branch ([`super::loop_meter`]) and exported as
//! `bun_agent_loop_turn_seconds`; any turn over 250 ms is logged with the
//! command or deploy op it ran.
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
use super::loop_meter::LoopBranch;
use super::probe::probe_health;
use super::supervisor::{WorkloadInstance, WorkloadSupervisor};
mod trace;
use trace::MAX_CONCURRENT_TRACES;
#[cfg(all(feature = "ebpf", target_os = "linux"))]
use trace::backend_addresses;
mod faults;
use faults::{InstalledNetworkFaults, NodeFaultFence};
mod deploy_worker;
use deploy_worker::DeployWorker;
mod deploy_ops;
use deploy_ops::{DeployOp, DeployOps, PreparedInstance, RollingInstance};

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

/// How often the agent loop runs its periodic health tick when nothing else
/// is waiting.
const HEALTH_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// The longest a steady stream of commands may hold off the health tick.
/// Probes, restarts and retirements stall for as long as it waits.
const HEALTH_TICK_STARVATION_BOUND: std::time::Duration = std::time::Duration::from_secs(5);

/// How long shutdown waits for node-pressure helpers to stop: one helper's
/// two-second exit wait plus its cgroup removal, with room to spare.
const SHUTDOWN_PRESSURE_CLEAR: std::time::Duration = std::time::Duration::from_secs(5);

/// Grace period between SIGTERM and SIGKILL during shutdown.
const SHUTDOWN_GRACE_SECS: u64 = 5;

/// How long an ordinary stop waits for a container to exit after SIGTERM
/// before it escalates to SIGKILL (DEP6).
const STOP_GRACE_SECS: u64 = 10;

/// How long a status answer waits for the runtime's pids and exit codes.
/// runc answers both under the instance's lifecycle lock, which a slow create
/// or stop can hold for seconds; status then reports what it knows and marks
/// the rest `runtime_unknown`, rather than holding the agent loop.
const STATUS_RUNTIME_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// How long one turn may wait, all told, on runtime reads and other work
/// it can retry on a later turn (#351, stage 3). Every such await in a turn
/// shares this one deadline, measured from the turn's start, so a turn that
/// meets a slow runtime many times still ends well inside the 1 s turn
/// budget. What doesn't finish in time fails the step, and the tick, the
/// deploy worker or the caller tries again.
const TURN_RUNTIME_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// How long a runtime read may take when no turn is running: during startup
/// adoption, which already gives each runtime inspection 10 s.
const OUTSIDE_TURN_RUNTIME_PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

/// At most this many instances' runtime reads run at once for one status.
const STATUS_RUNTIME_READ_CONCURRENCY: usize = 8;

/// How long one health tick may keep starting pending restarts. A restart
/// whose old runtime can't be cleaned up yet costs a few hundred
/// milliseconds, and a node that lost every container has dozens of them.
/// Walking them all in one tick held every queued command for seconds; now a
/// tick stops starting new ones once this is spent, and the next tick picks
/// up where it left off.
const PENDING_RESTART_TICK_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// The longest one confirmed stop can take with the production grace, given
/// the runtime's `stop_confirmation_timeout`: a drain of up to one grace, the
/// stop request, the grace itself, then the force-kill and its exit check.
/// Callers that wait for a stop or retirement size their deadline from this.
pub fn stop_completion_bound(confirmation_timeout: std::time::Duration) -> std::time::Duration {
    std::time::Duration::from_secs(STOP_GRACE_SECS) * 2 + confirmation_timeout * 3
}

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
    /// Whether instances adopted after a restart or self-upgrade already run
    /// exactly `spec` (replica count included), so the placement reconciler
    /// can record a still-pending placement as applied instead of rolling it.
    AdoptedPlacementMatches {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        response: oneshot::Sender<bool>,
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
        /// Members the API remembers as down. Gossip's live view no longer
        /// lists them, but the listing must show them as dead rather than
        /// drop them. The agent adds their council flags.
        down: Vec<NodeStatus>,
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
        /// Container mount path; required when several volumes share
        /// the snapshot name.
        volume: Option<String>,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Delete a snapshot.
    SnapshotDelete {
        namespace: String,
        app_name: String,
        name: String,
        /// Container mount path; required when several volumes share
        /// the snapshot name.
        volume: Option<String>,
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

impl AgentCommand {
    /// The variant's name, for the loop meter's slow-turn log.
    fn name(&self) -> &'static str {
        match self {
            AgentCommand::Deploy { .. } => "deploy",
            AgentCommand::RerunJobs { .. } => "rerun_jobs",
            AgentCommand::Stop { .. } => "stop",
            AgentCommand::Retire { .. } => "retire",
            AgentCommand::RetireTestResources { .. } => "retire_test_resources",
            AgentCommand::Status { .. } => "status",
            AgentCommand::AdoptedPlacementMatches { .. } => "adopted_placement_matches",
            AgentCommand::DesiredApps { .. } => "desired_apps",
            AgentCommand::ScrapeTargets { .. } => "scrape_targets",
            AgentCommand::CurrentResources { .. } => "current_resources",
            AgentCommand::JobStatus { .. } => "job_status",
            AgentCommand::ActiveImages { .. } => "active_images",
            AgentCommand::DeployOperations { .. } => "deploy_operations",
            AgentCommand::CancelDeploy { .. } => "cancel_deploy",
            AgentCommand::Logs { .. } => "logs",
            AgentCommand::FollowLogs { .. } => "follow_logs",
            AgentCommand::Exec { .. } => "exec",
            AgentCommand::Trace { .. } => "trace",
            AgentCommand::Nodes { .. } => "nodes",
            AgentCommand::Council { .. } => "council",
            AgentCommand::JoinIssue { .. } => "join_issue",
            AgentCommand::SnapshotCreate { .. } => "snapshot_create",
            AgentCommand::SnapshotList { .. } => "snapshot_list",
            AgentCommand::SnapshotRestore { .. } => "snapshot_restore",
            AgentCommand::SnapshotDelete { .. } => "snapshot_delete",
            AgentCommand::Resolve { .. } => "resolve",
            AgentCommand::ResolveAll { .. } => "resolve_all",
            AgentCommand::SyncClusterCatalog { .. } => "sync_cluster_catalog",
            AgentCommand::SyncClusterConsumer { .. } => "sync_cluster_consumer",
            AgentCommand::ConfirmConsumerReceipt { .. } => "confirm_consumer_receipt",
            AgentCommand::Routes { .. } => "routes",
            AgentCommand::PrepareNodeFault { .. } => "prepare_node_fault",
            AgentCommand::FenceNodeFault { .. } => "fence_node_fault",
            AgentCommand::InjectFault { .. } => "inject_fault",
            AgentCommand::ClearFault { .. } => "clear_fault",
            AgentCommand::ClearAllFaults { .. } => "clear_all_faults",
            AgentCommand::ClearFaultsByService { .. } => "clear_faults_by_service",
            AgentCommand::ListFaults { .. } => "list_faults",
            AgentCommand::SignImage { .. } => "sign_image",
            AgentCommand::AppConfig { .. } => "app_config",
            AgentCommand::UpgradeApply { .. } => "upgrade_apply",
            AgentCommand::UpgradeStatus { .. } => "upgrade_status",
            AgentCommand::UpgradeRollback { .. } => "upgrade_rollback",
            AgentCommand::UpgradeVerify { .. } => "upgrade_verify",
        }
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
    /// Some of the runtime's evidence for this instance (its liveness, pid
    /// or exit code) didn't arrive before the status deadline, so a `None`
    /// `pid` or `exit_code` is unknown rather than absent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub runtime_unknown: bool,
    /// How old the agent loop's published snapshot was when this answer was
    /// read from it, in milliseconds. Never more than two seconds: a node
    /// whose loop hasn't published for longer fails the request instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_age_ms: Option<u64>,
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

impl NodeStatus {
    /// Whether gossip has given up on this node: declared it dead, or seen it
    /// leave. The listing shows such nodes so people can see them; callers
    /// that want to talk to a node, or run work on it, skip them.
    pub fn is_down(&self) -> bool {
        matches!(self.state.as_str(), "dead" | "left")
    }
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
mod council_requests;
mod startup_recovery;
pub use consumer::ConsumerUpdate;
mod adopted_placements;
mod discovery_ownership;
mod discovery_recovery;
mod egress_ownership;
mod egress_resolution;
mod follow_ups;
mod identity_signing;
mod launch_evidence;
mod logs;
mod node_pressure_work;
mod off_loop_work;
mod producer_release;
mod restarts;
mod runtime_inventory;
mod scale_in_place;
mod signal_faults;
mod state_sweep;
mod status_snapshot;
use app_stop::{AppStop, PendingStops, StopPurpose};
use discovery_ownership::{DiscoveryOwnership, JournalReference};
use runtime_inventory::{LOOP_RUNTIME_INVENTORY_TIMEOUT, RUNTIME_INVENTORY_TIMEOUT};
pub use status_snapshot::{StatusReader, StatusUnavailable};

/// Test hook: an await on the loop that no mock stands behind (a
/// subprocess, the disk), which the starvation harness can slow down.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum LoopStall {
    /// Every fsync'd state persist: job ledger, instance record, schedules.
    Persist,
    /// Removing a retired instance's identity directory and record.
    ArtifactCleanup,
    /// The `nft -f -` subprocess that applies the perimeter ruleset.
    Firewall,
}

/// How long each [`LoopStall`] takes. Shared with the test through an `Arc`
/// because the agent moves into its task.
#[cfg(test)]
#[derive(Debug, Default)]
struct LoopStalls(std::sync::Mutex<std::collections::HashMap<LoopStall, std::time::Duration>>);

#[cfg(test)]
impl LoopStalls {
    fn set(&self, stall: LoopStall, delay: std::time::Duration) {
        if let Ok(mut stalls) = self.0.lock() {
            stalls.insert(stall, delay);
        }
    }

    /// Wait out `stall`'s delay, if the test set one; `true` when it did.
    async fn hold(&self, stall: LoopStall) -> bool {
        let delay = self
            .0
            .lock()
            .ok()
            .and_then(|stalls| stalls.get(&stall).copied());
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        delay.is_some()
    }
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
    /// Times every turn of `run_loop` by branch; the metrics collector
    /// exports it as `bun_agent_loop_turn_seconds`.
    loop_meter: Arc<super::loop_meter::LoopTurnMeter>,
    #[cfg(test)]
    loop_stalls: Arc<LoopStalls>,
    /// Hard per-node concurrency bound for workload connectivity traces.
    trace_slots: std::sync::Arc<tokio::sync::Semaphore>,
    volumes_dir: PathBuf,
    /// Which apps' volumes a snapshot operation owns right now.
    volume_maintenance: crate::bun::volume_maintenance::VolumeMaintenance,
    /// Test hook: an accepted restore waits here before touching the disk.
    #[cfg(test)]
    restore_pause: Option<std::sync::Arc<std::sync::Barrier>>,
    /// Test hook: a snapshot task waits here after it has answered, for
    /// as long as the test holds the write lock.
    #[cfg(test)]
    snapshot_answered_hold: Option<std::sync::Arc<tokio::sync::RwLock<()>>>,
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
    /// The node-pressure controller, which tasks lock to start and stop its
    /// helper off the loop.
    node_pressure: node_pressure_work::SharedPressure,
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
    /// Where the log store's checkpoint says each capture file was read up
    /// to when Bun started. Forwarders resume there instead of byte 0 (#308).
    capture_offsets: Arc<crate::ketchup::types::CaptureOffsets>,
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
    /// Where the last budget-bounded restart tick stopped, so the next one
    /// carries on from there instead of retrying the same few.
    restart_rotation: RestartRotation,
    /// Workload identity signings in flight, by the task running each.
    identity_signings: identity_signing::IdentitySignings,
    /// Those tasks. A follower's CSR waits on the leader for up to ten
    /// seconds, which must not stall every other command.
    identity_signing_tasks: tokio::task::JoinSet<identity_signing::SignedIdentity>,
    /// Apps adopted at startup and the spec their instances were launched
    /// from, until this agent deploys them again.
    adopted_apps: adopted_placements::AdoptedApps,
    /// What the loop knew at the end of its last turn, for status readers
    /// that must not queue behind it.
    status_tx: watch::Sender<Arc<status_snapshot::StatusSnapshot>>,
    /// The health tick's runtime state reads, at most one sweep at a time.
    state_sweeps: tokio::task::JoinSet<state_sweep::StateSweep>,
    /// Restarts with a runtime step in flight, by instance.
    restarts: restarts::Restarts,
    /// Those steps' tasks.
    restart_steps: tokio::task::JoinSet<restarts::StepResult>,
    /// Work a turn spawned and finishes when it reports back.
    follow_ups: tokio::task::JoinSet<follow_ups::FollowUp>,
    /// The upgrade or rollback preparing its binary, and its task, if one is.
    upgrade_preparing: Option<(follow_ups::UpgradeKind, tokio::task::Id)>,
    /// The task applying the perimeter ruleset with `nft`, if one is.
    firewall_applying: Option<tokio::task::Id>,
    /// When the turn in progress must stop waiting on work it can retry.
    turn_deadline: Option<tokio::time::Instant>,
    /// Whether the namespace-firewall maps missed a sync (a runtime that
    /// didn't name a workload's cgroup in time), so the tick retries it.
    namespace_firewall_stale: bool,
    /// The task re-resolving egress allowlists, if one is.
    #[cfg_attr(not(all(feature = "ebpf", target_os = "linux")), allow(dead_code))]
    egress_resolving: Option<tokio::task::Id>,
    /// Disk cleanup and provisioning running in tasks, which a later turn
    /// collects.
    off_loop_work: off_loop_work::OffLoopWork,
    /// Retirement's reads of a network reference, likewise (#387).
    network_reference_reads:
        off_loop_work::OffLoopWork<Option<crate::grill::runc_intent::NetworkReference>>,
}

/// The last instance each phase of `drive_pending_restarts` handled.
#[derive(Debug, Default)]
struct RestartRotation {
    /// Failed restarts whose partial runtime is still being cleaned up.
    cleanup: Option<InstanceId>,
    /// Pending instances being started again.
    launch: Option<InstanceId>,
}

/// Order `items` by instance id, starting just after `last`, so a tick that
/// runs out of budget part-way through leaves the rest for the next tick.
fn rotate_after<T>(
    mut items: Vec<T>,
    last: Option<&InstanceId>,
    id: impl Fn(&T) -> &InstanceId,
) -> Vec<T> {
    items.sort_by(|a, b| id(a).0.cmp(&id(b).0));
    if let Some(last) = last {
        let start = items.partition_point(|item| id(item).0 <= last.0);
        items.rotate_left(start);
    }
    items
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
            loop_meter: Arc::default(),
            #[cfg(test)]
            loop_stalls: Arc::default(),
            trace_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TRACES)),
            volumes_dir: crate::config::node::StorageSection::default().volumes,
            volume_maintenance: Default::default(),
            #[cfg(test)]
            restore_pause: None,
            #[cfg(test)]
            snapshot_answered_hold: None,
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
            node_pressure: Default::default(),
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
            capture_offsets: Arc::default(),
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
            restart_rotation: RestartRotation::default(),
            identity_signings: identity_signing::IdentitySignings::new(),
            identity_signing_tasks: tokio::task::JoinSet::new(),
            status_tx: watch::Sender::new(Arc::new(status_snapshot::StatusSnapshot::unpublished())),
            state_sweeps: tokio::task::JoinSet::new(),
            restarts: restarts::Restarts::new(),
            restart_steps: tokio::task::JoinSet::new(),
            follow_ups: tokio::task::JoinSet::new(),
            upgrade_preparing: None,
            firewall_applying: None,
            turn_deadline: None,
            namespace_firewall_stale: false,
            off_loop_work: off_loop_work::OffLoopWork::default(),
            network_reference_reads: off_loop_work::OffLoopWork::default(),
            egress_resolving: None,
            adopted_apps: adopted_placements::AdoptedApps::new(),
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
            loop_meter: Arc::default(),
            #[cfg(test)]
            loop_stalls: Arc::default(),
            trace_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TRACES)),
            volumes_dir: crate::config::node::StorageSection::default().volumes,
            volume_maintenance: Default::default(),
            #[cfg(test)]
            restore_pause: None,
            #[cfg(test)]
            snapshot_answered_hold: None,
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
            node_pressure: Default::default(),
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
            capture_offsets: Arc::default(),
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
            restart_rotation: RestartRotation::default(),
            identity_signings: identity_signing::IdentitySignings::new(),
            identity_signing_tasks: tokio::task::JoinSet::new(),
            status_tx: watch::Sender::new(Arc::new(status_snapshot::StatusSnapshot::unpublished())),
            state_sweeps: tokio::task::JoinSet::new(),
            restarts: restarts::Restarts::new(),
            restart_steps: tokio::task::JoinSet::new(),
            follow_ups: tokio::task::JoinSet::new(),
            upgrade_preparing: None,
            firewall_applying: None,
            turn_deadline: None,
            namespace_firewall_stale: false,
            off_loop_work: off_loop_work::OffLoopWork::default(),
            network_reference_reads: off_loop_work::OffLoopWork::default(),
            egress_resolving: None,
            adopted_apps: adopted_placements::AdoptedApps::new(),
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
    ///
    /// `resume` is the store's checkpoint as it opened: each forwarder
    /// starts its capture files there, so a restarted agent reads only the
    /// output the store doesn't hold yet.
    pub fn set_log_sink(
        &mut self,
        log_tx: mpsc::Sender<crate::ketchup::types::LogRecord>,
        resume: crate::ketchup::types::CaptureOffsets,
    ) {
        self.log_tx = Some(log_tx);
        self.capture_offsets = Arc::new(resume);
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
        // Configuration happens before the loop starts, so no task holds it.
        self.node_pressure
            .try_lock()
            .is_ok_and(|mut controller| controller.configure(limits, executable))
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

    /// Configure the actual protected listener ports, explicit enrolment peers
    /// and operator networks allowed to the management port. This grants
    /// network reachability only; protocol authentication still applies.
    pub fn configure_perimeter(
        &mut self,
        cluster_ports: Vec<u16>,
        management_port: u16,
        bootstrap_peers: Vec<std::net::IpAddr>,
        operator_cidrs: Vec<String>,
    ) {
        self.perimeter_config.cluster_ports = cluster_ports;
        self.perimeter_config.management_port = management_port;
        self.perimeter_config.bootstrap_peers = bootstrap_peers;
        self.perimeter_config.operator_cidrs = operator_cidrs;
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
        // Until a sync completes, the tick tries again (#351, stage 3).
        self.namespace_firewall_stale = true;
        let deadline = self.turn_deadline();
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
            let cgroup =
                tokio::time::timeout_at(deadline, self.supervisor.grill().workload_cgroup(&id))
                    .await;
            match cgroup {
                Ok(Ok(Some(cgroup))) => cgroup_ids.entry(key).or_default().push(cgroup),
                Ok(Ok(None)) => {}
                // Unavailable source evidence cannot authorise erasing
                // previously installed namespace/firewall bindings.
                Ok(Err(error)) => {
                    eprintln!("sesame: source identity for {id} is unavailable: {error}");
                    return;
                }
                Err(_) => {
                    eprintln!(
                        "sesame: source identity for {id} did not arrive within the turn; the next tick retries"
                    );
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
        } else {
            self.namespace_firewall_stale = false;
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

    /// The loop's turn meter, for the metrics collector to export.
    pub fn loop_meter(&self) -> Arc<super::loop_meter::LoopTurnMeter> {
        Arc::clone(&self.loop_meter)
    }

    /// Attach the self-upgrade manager (enables the upgrade commands).
    pub fn set_upgrade_manager(&mut self, manager: crate::upgrade::manager::UpgradeManager) {
        self.upgrade = Some(manager);
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
            #[cfg(test)]
            self.loop_stalls.hold(LoopStall::Persist).await;
            // LOOP-INLINE: fsync'd persist (#351 decision 2); the slow-disk scenario bounds it
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
        // An earlier attempt of this apply may already be clearing the
        // previous run off the loop, and marked it Stopping to do so.
        let clearing = self.off_loop_work.started(
            &off_loop_work::WorkKey::ClearJobRun(id.clone()),
            self.incarnation_of(&id),
        );
        if let Some(instance) = self.supervisor.get_instance(&id) {
            if !instance.is_job || instance.app_name != name || instance.namespace != namespace {
                return Err(refuse("instance id belongs to another workload"));
            }
            if !rerun_unknown
                && !clearing
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
                self.clear_previous_job_run(&id).await?;
            }
            self.record_job_runtime_absent(&id).await?;
            self.retire_instance_artifacts(&id).await?;
            // LOOP-INLINE: in-memory lock, no I/O
            self.supervisor.retire_instance(&id).await;
        }
        // LOOP-INLINE: in-memory lock, no I/O
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
    async fn persist_instance_record(
        &self,
        instance_id: &InstanceId,
        evidence: &launch_evidence::LaunchEvidence,
    ) -> Result<(), BunError> {
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
            evidence.pid
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
            log_stem: evidence.log_stem.clone(),
            host_port: instance.host_port,
            app_spec: self
                .deployed_specs
                .get(&(instance.app_name.clone(), instance.namespace.clone()))
                .cloned(),
            oci_spec,
            rootless_network: evidence.rootless_network.clone(),
        };
        #[cfg(test)]
        self.loop_stalls.hold(LoopStall::Persist).await;
        // LOOP-INLINE: fsync'd persist (#351 decision 2); the slow-disk scenario bounds it
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
        let runtime = self.supervisor.grill().runtime_kind();
        let pid = if runtime == crate::grill::records::RuntimeKind::Apple {
            Some(std::process::id())
        } else {
            instance.launch.pid
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
            log_stem: instance.launch.log_stem.clone(),
            host_port: instance.host_port,
            app_spec: Some(instance.spec.clone()),
            oci_spec: instance.oci_spec.clone(),
            rootless_network: instance.launch.rootless_network.clone(),
        };
        // LOOP-INLINE: fsync'd persist (#351 decision 2); the slow-disk scenario bounds it
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
                        match self.supervisor.grill().exit_code(id).await? {
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
                self.retire_instance_artifacts_fully(&id).await?;
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
                            match self.supervisor.grill().exit_code(&runtime_id).await? {
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
                    self.retire_instance_artifacts_fully(&runtime_id).await?;
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
            if !record.is_job {
                self.note_adopted_instance(
                    &key,
                    &instance_id,
                    record.app_spec.as_ref(),
                    &record.image,
                );
            }
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
        // Producer: the runtime streams complete lines into line_tx, starting
        // each capture file where the store's checkpoint stopped (#308). An
        // adopted instance's earlier output is neither re-read nor, should a
        // line come round again, ingested twice: the store drops it.
        let follow_grill = grill;
        let follow_id = id.clone();
        let resume = Arc::clone(&self.capture_offsets);
        tokio::spawn(async move {
            follow_grill.follow_logs(&follow_id, line_tx, &resume).await;
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
        let mut health_interval = tokio::time::interval(HEALTH_TICK_INTERVAL);
        let mut last_health_tick = tokio::time::Instant::now();

        if let Some(readiness) = self.readiness.clone() {
            let (capabilities, _) = self.live_egress_report_state().await;
            // LOOP-INLINE: in-memory lock, no I/O
            readiness.set_capabilities(capabilities).await;
        }

        self.publish_status();
        if let Some(ready) = ready {
            ready.ready();
        }

        loop {
            // Commands come before a tick that is merely due (#260), but a
            // steady stream of them must not hold the tick off for good:
            // health probes, restarts and retirements all run from it.
            let turn = if last_health_tick.elapsed() >= HEALTH_TICK_STARVATION_BOUND {
                let turn = self.begin_turn(LoopBranch::HealthTick, None);
                self.run_health_tick().await;
                last_health_tick = tokio::time::Instant::now();
                health_interval.reset();
                turn
            } else {
                // Branches are polled in order, so the periodic tick runs only
                // when nothing else is waiting. A tick can take seconds (every
                // pending restart retries its runtime cleanup), and ticks that
                // fall behind are due at once. Polled in random order, each
                // queued command had to win a coin toss against the next slow
                // tick; callers timed out, the consumer view that would let
                // restarts finish never landed, and the node stopped
                // answering. Now a command waits for at most the tick already
                // running.
                //
                // Only branches that can't flood sit above commands. The
                // report worker asks for a snapshot once per interval. Stop
                // completions come from stops already started, one each, and
                // identity signings from provisions already started, at most
                // one per instance. Restart steps are one per restart in
                // flight (`RESTARTS_IN_FLIGHT_LIMIT` at most), and the tick
                // runs one state sweep at a time. Every deploy op comes from a deploy task
                // that waits for its reply before sending the next, so no more
                // of them wait than there are deploys and probes in flight. Commands come from
                // any number of callers taking turns; during a `relish test`
                // pulse the channel is never empty, and below it a deploy's
                // first step waited out its 300 s deadline.
                tokio::select! {
                    biased;
                    _ = self.shutdown.cancelled() => {
                        self.abandon_pending_stops();
                        self.abandon_identity_signings();
                        // An upgrade still preparing leaves its caller a
                        // closed channel, and the node on its current binary.
                        self.follow_ups.abort_all();
                        self.off_loop_work.abandon_all();
                        self.network_reference_reads.abandon_all();
                        self.shutdown_all().await;
                        break;
                    }
                    Some(req) = Self::recv_snapshot(&mut self.cluster) => {
                        let turn = self.begin_turn(LoopBranch::Snapshot, None);
                        self.handle_snapshot_request(req).await;
                        turn
                    }
                    Some(outcome) = self.stop_waits.join_next_with_id(),
                        if !self.stop_waits.is_empty() => {
                        let turn = self.begin_turn(LoopBranch::StopWait, None);
                        self.complete_app_stop(outcome).await;
                        turn
                    }
                    Some(outcome) = self.identity_signing_tasks.join_next_with_id(),
                        if !self.identity_signing_tasks.is_empty() => {
                        let turn = self.begin_turn(LoopBranch::IdentitySigning, None);
                        self.finish_identity_provision(outcome);
                        turn
                    }
                    Some(outcome) = self.restart_steps.join_next_with_id(),
                        if !self.restart_steps.is_empty() => {
                        let turn = self.begin_turn(LoopBranch::RestartStep, None);
                        self.finish_restart_step(outcome).await;
                        turn
                    }
                    Some(sweep) = self.state_sweeps.join_next(),
                        if !self.state_sweeps.is_empty() => {
                        let turn = self.begin_turn(LoopBranch::StateSweep, None);
                        self.apply_state_sweep(sweep).await;
                        turn
                    }
                    Some(done) = self.follow_ups.join_next_with_id(),
                        if !self.follow_ups.is_empty() => {
                        let turn = self.begin_turn(LoopBranch::FollowUp, None);
                        self.apply_follow_up(done).await;
                        turn
                    }
                    Some(op) = self.deploy_ops_rx.recv() => {
                        let turn = self.begin_turn(LoopBranch::DeployOp, Some(op.name()));
                        self.handle_deploy_op(op).await;
                        turn
                    }
                    Some(cmd) = self.command_rx.recv() => {
                        let turn = self.begin_turn(LoopBranch::Command, Some(cmd.name()));
                        self.handle_command(cmd).await;
                        turn
                    }
                    _ = health_interval.tick() => {
                        let turn = self.begin_turn(LoopBranch::HealthTick, None);
                        self.run_health_tick().await;
                        last_health_tick = tokio::time::Instant::now();
                        turn
                    }
                }
            };
            // Local changes only mark the consumer view stale, so a burst of
            // them costs one republication, and the old view serves meanwhile.
            // The republication is part of the turn: a caller waits for it too.
            if let Err(error) = self.refresh_consumer_view().await {
                eprintln!("bun: consumer view refresh awaits retry: {error}");
            }
            // Status readers answer from this, not by queueing for a turn.
            self.publish_status();
            self.turn_deadline = None;
            self.loop_meter.finish(turn);
        }
    }

    /// Start timing a turn, and start the clock on what it may spend
    /// waiting for work it can retry ([`TURN_RUNTIME_BUDGET`]).
    fn begin_turn(
        &mut self,
        branch: LoopBranch,
        detail: Option<&'static str>,
    ) -> super::loop_meter::Turn {
        self.turn_deadline = Some(tokio::time::Instant::now() + TURN_RUNTIME_BUDGET);
        self.loop_meter.begin(branch, detail)
    }

    /// When the turn in progress must give up on a runtime read or other
    /// work it can retry later. Outside a turn (startup adoption, or a test
    /// driving a handler directly) nothing else is waiting, so each wait gets
    /// [`OUTSIDE_TURN_RUNTIME_PATIENCE`] instead.
    fn turn_deadline(&self) -> tokio::time::Instant {
        self.turn_deadline
            .unwrap_or_else(|| tokio::time::Instant::now() + OUTSIDE_TURN_RUNTIME_PATIENCE)
    }

    /// The loop's periodic work: health probes, restarts, retirements, jobs,
    /// fault expiry, firewall reconciliation and identity rotation.
    async fn run_health_tick(&mut self) {
        self.reopen_uncertain_discovery().await;
        if let Err(error) = self.fence_lapsed_view().await {
            eprintln!("bun: withdrawing the lapsed cluster view awaits retry: {error}");
        }
        self.drive_startup_retirements().await;
        self.drive_deferred_retirements().await;
        self.refresh_egress_readiness().await;
        self.run_health_checks().await;
        self.fire_due_jobs().await;
        self.begin_state_sweep();
        self.drive_pending_restarts().await;
        self.expire_faults().await;
        self.reconcile_firewall();
        if self.namespace_firewall_stale {
            self.sync_firewall_ebpf().await;
        }
        self.reresolve_egress();
        self.sweep_kernel_networking().await;
        self.check_identity_rotation();
    }

    /// Enforce the current kernel boundary and publish this tick's capabilities.
    async fn refresh_egress_readiness(&mut self) {
        let egress = self.enforce_live_egress_or_stop().await;
        if let Some(readiness) = self.readiness.clone() {
            // LOOP-INLINE: in-memory lock, no I/O
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

        // The worker gave up on this one; building it would only delay the
        // next live request by another inventory read.
        if req.response.is_closed() {
            return;
        }
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
        // LOOP-INLINE: in-memory lock, no I/O
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
    ///
    /// Lists gossip's live members (alive and suspect), then each of `down`
    /// that gossip doesn't list any more, so a dead node shows as dead
    /// instead of disappearing.
    fn get_cluster_nodes(&self, down: Vec<NodeStatus>) -> Vec<NodeStatus> {
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
        // Raft metrics are authoritative when the council is wired;
        // otherwise fall back to whatever the gossip snapshot reports. A
        // dead voter is still a voter, so down members get the same check.
        let roles = |name: &str, gossip_council: bool, gossip_leader: bool| {
            if have_metrics {
                (
                    council_names.contains(name),
                    leader_name.as_deref() == Some(name),
                )
            } else {
                (gossip_council, gossip_leader)
            }
        };
        let membership = handle.membership_rx.borrow();
        let mut nodes: Vec<NodeStatus> = membership
            .iter()
            .map(|m| {
                let name = m.node_id.to_string();
                let (is_council, is_leader) = roles(&name, m.is_council, m.is_leader);
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
            .collect();
        for mut node in down {
            if nodes.iter().any(|live| live.node_id == node.node_id) {
                continue;
            }
            (node.is_council, node.is_leader) = roles(&node.node_id, false, false);
            nodes.push(node);
        }
        nodes
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
        // LOOP-INLINE: reads the local council state machine; no quorum round trip
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

    /// Reserve an app's volumes for a snapshot operation.
    fn reserve_volumes(
        &mut self,
        namespace: &str,
        app: &str,
        operation: crate::bun::volume_maintenance::VolumeOperation,
    ) -> Option<crate::bun::volume_maintenance::VolumeLease> {
        self.volume_maintenance.reserve(namespace, app, operation)
    }

    /// Hand a snapshot operation's volumes back, then answer it. The order
    /// matters: once the caller has the answer it may send its next
    /// snapshot request straight away, and that request must not find this
    /// finished operation still holding the volumes (#340). The work is done
    /// by now, so releasing first can't let anything overlap it.
    fn release_then_answer<T>(
        lease: crate::bun::volume_maintenance::VolumeLease,
        response: oneshot::Sender<Result<T, BunError>>,
        result: Result<T, BunError>,
    ) {
        drop(lease);
        let _ = response.send(result);
    }

    /// Test hook: park a snapshot task that has already answered until
    /// the test releases its write lock on `hold`.
    #[cfg(test)]
    fn hold_after_answer(hold: Option<&tokio::sync::RwLock<()>>) {
        if let Some(hold) = hold {
            let _parked = hold.blocking_read();
        }
    }

    fn volumes_busy(namespace: &str, app: &str) -> BunError {
        crate::grill::snapshot::SnapshotError::Busy {
            namespace: namespace.to_string(),
            app: app.to_string(),
        }
        .into()
    }

    /// The first app in `config` whose volumes a restore owns.
    fn restoring_target(&self, config: &Config) -> Option<(String, String)> {
        config.app.iter().find_map(|(name, spec)| {
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            self.volume_maintenance
                .restoring(namespace, name)
                .then(|| (namespace.to_string(), name.clone()))
        })
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
            // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
            let _ = events
                .send(ApplyEvent::Error {
                    message: "startup cleanup still owns runtime allocations; retry after recovery"
                        .into(),
                })
                .await;
            return;
        }
        if rerun_unknown_jobs && let Err(message) = super::jobs::validate_rerun(&config) {
            // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
            let _ = events
                .send(ApplyEvent::Error {
                    message: message.into(),
                })
                .await;
            return;
        }
        if let Err(message) = self.validate_deploy_names(&config) {
            // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
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
            // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
            let _ = events.send(ApplyEvent::Error { message }).await;
            return;
        }
        if let Some((namespace, app)) = self.restoring_target(&config) {
            let message = format!(
                "volumes of {namespace}/{app} are being restored from a snapshot; retry once the restore finishes"
            );
            // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
            let _ = events.send(ApplyEvent::Error { message }).await;
            return;
        }
        // LOOP-INLINE: in-memory lock, no I/O
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
                // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
                let _ = events.send(ApplyEvent::Error { message }).await;
                return;
            }
        };
        // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
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
            // LOOP-INLINE: in-memory lock, no I/O
            operation
                .finish(
                    crate::bun::deploy_operations::DeployOperationOutcome::Failed,
                    message.clone(),
                )
                .await;
            // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
            let _ = events.send(ApplyEvent::Error { message }).await;
            return;
        }
        // Register any cron-scheduled jobs so the event loop fires them
        // on their schedule rather than at deploy time (E).
        if register_schedule && let Err(error) = self.register_scheduled_jobs(&config).await {
            let message = error.to_string();
            // LOOP-INLINE: in-memory lock, no I/O
            operation
                .finish(
                    crate::bun::deploy_operations::DeployOperationOutcome::Failed,
                    message.clone(),
                )
                .await;
            // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
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
                // Publish now so the answer reflects every earlier command,
                // and read the runtime's evidence off the loop.
                let snapshot = self.publish_status();
                let grill = self.supervisor.grill().clone();
                tokio::spawn(async move {
                    let statuses = status_snapshot::read_status(&grill, &snapshot).await;
                    let _ = response.send(statuses);
                });
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
            AgentCommand::AdoptedPlacementMatches {
                app_name,
                namespace,
                spec,
                response,
            } => {
                let _ = response.send(self.adopted_instances_match(&app_name, &namespace, &spec));
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
                            blocked: None,
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
                // LOOP-INLINE: in-memory lock, no I/O
                let operation = self
                    .deploy_operations
                    .request_cancellation(&operation_id)
                    .await;
                let _ = response.send(operation);
            }
            AgentCommand::DeployOperations { response } => {
                // LOOP-INLINE: in-memory lock, no I/O
                let _ = response.send(self.deploy_operations.snapshot().await);
            }
            AgentCommand::Logs {
                app_name,
                namespace,
                tail,
                response,
            } => {
                self.spawn_logs_read(&app_name, &namespace, tail, response);
            }
            AgentCommand::FollowLogs {
                app_name,
                namespace,
                tail,
                label,
                lines,
            } => {
                self.spawn_logs_follow(&app_name, &namespace, tail, label, lines);
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
            AgentCommand::Nodes { down, response } => {
                let nodes = self.get_cluster_nodes(down);
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
                self.spawn_join_issue(token, node_id, csr_der, response);
            }
            AgentCommand::SnapshotCreate {
                namespace,
                app_name,
                volume,
                name,
                response,
            } => {
                let Some(lease) = self.reserve_volumes(
                    &namespace,
                    &app_name,
                    crate::bun::volume_maintenance::VolumeOperation::Snapshot,
                ) else {
                    let _ = response.send(Err(Self::volumes_busy(&namespace, &app_name)));
                    return;
                };
                // btrfs subprocess + fs walks off the command loop (M7).
                let volumes_dir = self.volumes_dir.clone();
                #[cfg(test)]
                let hold = self.snapshot_answered_hold.clone();
                tokio::task::spawn_blocking(move || {
                    let result = crate::grill::snapshot::SnapshotManager::new(&volumes_dir)
                        .create_for_app(
                            &namespace,
                            &app_name,
                            volume.as_deref(),
                            name.as_deref(),
                            std::time::SystemTime::now(),
                        )
                        .map_err(BunError::from);
                    Self::release_then_answer(lease, response, result);
                    #[cfg(test)]
                    Self::hold_after_answer(hold.as_deref());
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
                volume,
                response,
            } => {
                // The running-instance check needs supervisor state, so it stays
                // on the loop; the btrfs restore itself runs off it (M7). An
                // instance waiting to be restarted counts as running.
                let running = self.supervisor.list_instances().into_iter().any(|i| {
                    i.app_name == app_name
                        && i.namespace == namespace
                        && (i.retry_pending
                            || !matches!(i.state, ContainerState::Stopped | ContainerState::Failed))
                });
                if running {
                    let _ = response.send(Err(crate::grill::snapshot::SnapshotError::AppRunning {
                        namespace: namespace.clone(),
                        app: app_name.clone(),
                    }
                    .into()));
                    return;
                }
                // A deploy still creating the volumes off the loop would
                // race the restore's swap.
                if self.off_loop_work.provisioning(&namespace, &app_name) {
                    let _ = response.send(Err(Self::volumes_busy(&namespace, &app_name)));
                    return;
                }
                // Reserve before dispatching, with no await in between: from
                // here until the task drops the lease, deploys, restarts and
                // other snapshot operations on this app are refused (B03).
                let Some(lease) = self.reserve_volumes(
                    &namespace,
                    &app_name,
                    crate::bun::volume_maintenance::VolumeOperation::Restore,
                ) else {
                    let _ = response.send(Err(Self::volumes_busy(&namespace, &app_name)));
                    return;
                };
                let volumes_dir = self.volumes_dir.clone();
                #[cfg(test)]
                let pause = self.restore_pause.clone();
                #[cfg(test)]
                let hold = self.snapshot_answered_hold.clone();
                tokio::task::spawn_blocking(move || {
                    #[cfg(test)]
                    if let Some(pause) = pause {
                        pause.wait();
                    }
                    let result = crate::grill::snapshot::SnapshotManager::new(&volumes_dir)
                        .restore(&namespace, &app_name, &name, volume.as_deref())
                        .map_err(BunError::from);
                    Self::release_then_answer(lease, response, result);
                    #[cfg(test)]
                    Self::hold_after_answer(hold.as_deref());
                });
            }
            AgentCommand::SnapshotDelete {
                namespace,
                app_name,
                name,
                volume,
                response,
            } => {
                let Some(lease) = self.reserve_volumes(
                    &namespace,
                    &app_name,
                    crate::bun::volume_maintenance::VolumeOperation::Snapshot,
                ) else {
                    let _ = response.send(Err(Self::volumes_busy(&namespace, &app_name)));
                    return;
                };
                let volumes_dir = self.volumes_dir.clone();
                #[cfg(test)]
                let hold = self.snapshot_answered_hold.clone();
                tokio::task::spawn_blocking(move || {
                    let result = crate::grill::snapshot::SnapshotManager::new(&volumes_dir)
                        .delete(&namespace, &app_name, &name, volume.as_deref())
                        .map_err(BunError::from);
                    Self::release_then_answer(lease, response, result);
                    #[cfg(test)]
                    Self::hold_after_answer(hold.as_deref());
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
                match self
                    .fence_node_fault_up_to_pressure(&reservation, only_if_finished)
                    .await
                {
                    Ok(NodeFaultFence::Done) => {
                        let _ = response.send(Ok(()));
                    }
                    Ok(NodeFaultFence::Pressure { fenced }) => {
                        self.spawn_node_pressure_fence(reservation, fenced, response);
                    }
                    Err(reason) => {
                        let _ = response.send(Err(BunError::FaultRejected { reason }));
                    }
                }
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
                // A kill, pause or resume reads its targets' pids and signals
                // them from a task; the caller hears once they're signalled.
                if let Some(signal) = signal_faults::Signal::of(&rule.fault_type) {
                    self.spawn_signal_fault(&rule, signal, response);
                    return;
                }
                // A pressure helper takes seconds to start; it starts in a
                // task, and the caller hears once it runs (#351, stage 3).
                if let crate::smoker::types::FaultType::NodePressure {
                    cpu_percentage,
                    memory_percentage,
                } = rule.fault_type
                {
                    match self.check_node_pressure(&rule, cpu_percentage, memory_percentage) {
                        Ok(()) => self.spawn_node_pressure_start(
                            rule.id,
                            cpu_percentage,
                            memory_percentage,
                            response,
                        ),
                        Err(reason) => {
                            self.fault_registry.remove(rule.id);
                            let _ = response.send(Err(BunError::FaultRejected { reason }));
                        }
                    }
                    return;
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
                            // The helper stops in a task, and the caller hears
                            // once it's gone (#351, stage 3).
                            self.spawn_node_pressure_clearance(rule, reservation, response);
                            return;
                        }
                        self.reverse_fault(&rule).await;
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
                self.spawn_sign_image(submission, response);
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
                self.begin_upgrade_apply(directive, response);
            }
            AgentCommand::UpgradeStatus { response } => {
                let result = match &self.upgrade {
                    Some(manager) => Ok(manager.status()),
                    None => Err(BunError::UpgradesUnavailable),
                };
                let _ = response.send(result);
            }
            AgentCommand::UpgradeRollback { version, response } => {
                self.begin_upgrade_rollback(version, response);
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
        // LOOP-INLINE: reads the local council state machine; no quorum round trip
        let catalog = council.manifest_catalog().await;
        // LOOP-INLINE: reads the local council state machine; no quorum round trip
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
        // LOOP-INLINE: reads the local council state machine; no quorum round trip
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
        // Pending → Preparing. Storage provisioning below can outlast the
        // turn and answer `StillRunning`; the deploy worker then asks again
        // for the same incarnation, which is already Preparing (#386).
        {
            let instance = self
                .supervisor
                .get_instance_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;
            if instance.state != ContainerState::Preparing {
                instance.state = instance.state.transition_to(ContainerState::Preparing)?;
            }
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
        evidence: &launch_evidence::LaunchEvidence,
    ) -> Result<(), BunError> {
        self.spawn_log_forwarder(instance_id, app_name, namespace);
        self.persist_instance_record(instance_id, evidence).await?;
        let container_ip = evidence.container_ip;

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
            // LOOP-INLINE: in-memory lock, no I/O
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
                // The tick finishes either: one waits on the leader, the
                // other on disk cleanup running off the loop.
                Err(BunError::ProducerReleasePending { .. } | BunError::StillRunning { .. }) => {
                    self.defer_retirement(old_id)
                }
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
        // The rolled-out spec owns the route now (#307): a changed host
        // replaces the old one, and a spec without ingress drops it. The
        // stopped-app restore before the rollout only inserts when nothing
        // is stored, so this is where a running app's route changes.
        let key = (namespace.to_string(), app_name.to_string());
        match &spec.ingress {
            Some(ingress) => {
                self.ingress_configs.insert(key, ingress.clone());
            }
            None => {
                self.ingress_configs.remove(&key);
            }
        }
        self.rebuild_routing_table().await;

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
        evidence: &launch_evidence::LaunchEvidence,
    ) -> Result<(), BunError> {
        if let Some(instance) = self.supervisor.get_instance_mut(instance_id) {
            instance.oci_spec = Some(oci_spec);
        }
        self.spawn_log_forwarder(instance_id, job_name, namespace);
        self.persist_instance_record(instance_id, evidence).await?;
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
        retained: Result<Option<crate::grill::runc_intent::NetworkReference>, BunError>,
    ) -> Result<(), BunError> {
        self.retain_network_reference(instance_id, spec, retained)
            .await?;
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
            // LOOP-INLINE: one cgroupfs mkdir, microseconds
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

    /// Record the network reference the runtime retained for `id` before it
    /// starts. Whoever created the instance asked the runtime for it, off
    /// the loop (#351, stage 3); the loop checks it belongs to `id`'s
    /// generation and journals it.
    async fn retain_network_reference(
        &mut self,
        id: &InstanceId,
        spec: Option<&AppSpec>,
        retained: Result<Option<crate::grill::runc_intent::NetworkReference>, BunError>,
    ) -> Result<(), BunError> {
        let retained = retained?;
        if !launch_evidence::retains_network(spec) {
            // The spec stopped publishing an address after the retain.
            if let Some(reference) = retained {
                self.hand_back_network_reference(reference);
            }
            return Ok(());
        }
        if let Some(reference) = retained {
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
            if let Err(error) = self.persist_discovery_reference(&reference).await {
                // A refusal decided in memory never reached the journal, so no
                // publication can name this address yet. Hand it back now rather
                // than leave a hold nothing tracks. After an uncertain write the
                // journal may record it, so only retirement may release it.
                if !matches!(self.discovery_ownership, DiscoveryOwnership::Uncertain) {
                    self.hand_back_network_reference(reference);
                }
                return Err(error);
            }
            self.network_references.insert(id.clone(), reference);
        }
        Ok(())
    }

    /// Give back a hold no journal records, from a task: nothing waits on
    /// it, and a release names its generation, so it can't touch a later
    /// retain of the same instance.
    fn hand_back_network_reference(&self, reference: crate::grill::runc_intent::NetworkReference) {
        let grill = self.supervisor.grill().clone();
        tokio::spawn(async move {
            if let Err(error) = grill.release_network_reference(&reference).await {
                eprintln!(
                    "bun: handing back {}'s untracked network reference failed: {error}",
                    reference.instance_id
                );
            }
        });
    }

    async fn release_network_reference(
        &mut self,
        id: &InstanceId,
        remote: Option<&crate::onion::producer::ProducerReleaseConfirmation>,
    ) -> Result<(), BunError> {
        // The runtime answers these under the instance's lifecycle lock, and
        // on runc the health sweep's and status reader's state reads queue
        // for it too, so either call can take longer than a turn (#387). Each
        // runs in a task: one that hasn't answered within the turn fails the
        // retirement with `StillRunning`, and the retry collects the same
        // task instead of asking again. A release is idempotent and names its
        // generation, so one that lands late is harmless.
        let reference = match self.network_references.get(id).cloned() {
            Some(reference) => reference,
            None => {
                let Some(held) = self.read_network_reference(id).await? else {
                    return Ok(());
                };
                match self.journal_reference(&held) {
                    // The hold was retained but its launch never recorded it, so
                    // no publication ever named the address: nothing to withdraw.
                    JournalReference::Unrecorded => {
                        return self.finish_network_release(id, held).await;
                    }
                    // Recorded by a write whose outcome was uncertain at the time.
                    JournalReference::Recorded => {
                        self.network_references.insert(id.clone(), held.clone());
                        held
                    }
                    JournalReference::Unknown => {
                        return Err(BunError::RetirementState {
                            instance_id: id.clone(),
                            reason: "retained network reference requires original discovery reconciliation"
                                .into(),
                        });
                    }
                }
            }
        };
        self.authorise_local_discovery_release(&reference, remote)
            .await?;
        self.require_discovery_release_permission(&reference)?;
        self.finish_network_release(id, reference.clone()).await?;
        self.forget_released_discovery_reference(&reference).await?;
        self.network_references.remove(id);
        Ok(())
    }

    /// Which network reference the runtime holds for `id`, read in a task
    /// ([`off_loop_work`]). `StillRunning` means ask again.
    async fn read_network_reference(
        &mut self,
        id: &InstanceId,
    ) -> Result<Option<crate::grill::runc_intent::NetworkReference>, BunError> {
        let grill = self.supervisor.grill().clone();
        let read_id = id.clone();
        let read = async move {
            grill
                .network_reference(&read_id)
                .await
                .map_err(|error| error.to_string())
        };
        let key = off_loop_work::WorkKey::ReadNetworkReference(id.clone());
        let incarnation = self.incarnation_of(id);
        let turn_deadline = self.turn_deadline();
        // LOOP-INLINE: `finish` waits with `timeout_at(turn_deadline)`
        self.network_reference_reads
            .finish(key, incarnation, read, turn_deadline)
            .await?
            .map_err(|reason| BunError::RetirementState {
                instance_id: id.clone(),
                reason,
            })
    }

    /// Hand `reference` back to the runtime from a task ([`off_loop_work`]).
    /// `StillRunning` means ask again.
    async fn finish_network_release(
        &mut self,
        id: &InstanceId,
        reference: crate::grill::runc_intent::NetworkReference,
    ) -> Result<(), BunError> {
        let grill = self.supervisor.grill().clone();
        let release = async move {
            grill
                .release_network_reference(&reference)
                .await
                .map_err(|error| error.to_string())
        };
        let key = off_loop_work::WorkKey::ReleaseNetworkReference(id.clone());
        let incarnation = self.incarnation_of(id);
        self.finish_off_loop_work(key, incarnation, release)
            .await?
            .map_err(|reason| BunError::RetirementState {
                instance_id: id.clone(),
                reason,
            })
    }

    /// A build without the eBPF data path cannot enforce an allowlist.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn apply_network_pre_start(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        spec: Option<&AppSpec>,
        _cgroup_path: &std::path::Path,
        retained: Result<Option<crate::grill::runc_intent::NetworkReference>, BunError>,
    ) -> Result<(), BunError> {
        self.retain_network_reference(instance_id, spec, retained)
            .await?;
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
        // LOOP-INLINE: reads /proc boot_id, microseconds
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
        // LOOP-INLINE: reads /proc boot_id, microseconds
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
        // LOOP-INLINE: in-memory lock, no I/O
        self.supervisor.stop_app(app_name, namespace).await?;
        let mut first_error = None;
        let deadline = self.turn_deadline();
        for (id, publishes_address) in instances {
            let result = async {
                if publishes_address {
                    let reference = tokio::time::timeout_at(
                        deadline,
                        self.supervisor.grill().network_reference(&id),
                    )
                    .await
                    .map_err(|_| BunError::RetirementState {
                        instance_id: id.clone(),
                        reason: "the runtime did not name the retained address within the turn"
                            .into(),
                    })??;
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
                // The kill runs off the loop; until it's confirmed the fence
                // reports itself unconfirmed, and the next egress check
                // fences again and collects it.
                self.kill_off_the_loop(off_loop_work::WorkKey::FenceExecution(id.clone()), &id)
                    .await?
                    .map_err(|reason| BunError::RetirementState {
                        instance_id: id.clone(),
                        reason: format!("execution fence kill unconfirmed: {reason}"),
                    })
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
    ///
    /// The lookups run in a task ([`egress_resolution`]); the loop applies
    /// what they found when the task reports back, and starts no second
    /// re-resolution meanwhile.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    fn reresolve_egress(&mut self) {
        if self.egress_store_uncertain || self.egress_resolving.is_some() {
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
        let requests = egress_resolution::requests(self.egress_bindings.iter());
        let task = self.follow_ups.spawn(async move {
            follow_ups::FollowUp::EgressResolved(egress_resolution::resolve(requests).await)
        });
        self.egress_resolving = Some(task.id());
    }

    /// No-op without the eBPF data path.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    fn reresolve_egress(&mut self) {}

    /// Record re-resolved allowlists, and reprogram the cgroups whose
    /// destinations changed, for the bindings they still describe.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn apply_egress_resolutions(&mut self, resolutions: Vec<egress_resolution::Resolution>) {
        if self.egress_store_uncertain {
            return;
        }
        for resolution in resolutions {
            let request = resolution.request;
            let new_resolved = match resolution.resolved {
                Ok(resolved) => resolved,
                Err(error) => {
                    eprintln!(
                        "sesame: egress re-resolve failed for {}: {error}",
                        request.instance_id.0
                    );
                    continue;
                }
            };
            let Some(binding) = self.egress_bindings.get_mut(&request.instance_id) else {
                continue;
            };
            if !egress_resolution::still_current(Some(&*binding), &request) {
                continue;
            }
            let (to_add, to_remove) =
                crate::sesame::egress::egress_diff(&binding.resolved, &new_resolved);
            if to_add.is_empty() && to_remove.is_empty() {
                continue;
            }

            // Record the new set, then rebuild the cgroup's kernel state
            // from all bindings: CIDR values are merged per cgroup, so a
            // delta write can't be applied entry by entry.
            binding.resolved = new_resolved;
            if let Err(error) = self.reprogram_cgroup_egress(request.cgroup_id, None).await {
                self.handle_egress_rewrite_failure(request.cgroup_id, error)
                    .await;
            }
        }
    }

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
        // LOOP-INLINE: in-memory lock, no I/O
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
            #[cfg(test)]
            self.loop_stalls.hold(LoopStall::Persist).await;
            // LOOP-INLINE: fsync'd persist (#351 decision 2); the slow-disk scenario bounds it
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
        // LOOP-INLINE: in-memory lock, no I/O
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

    /// The instances whose runtime state a sweep should read: running apps
    /// (to catch a crash that no health check would) and running jobs that
    /// haven't recorded an exit yet, filtered by `include`.
    fn plan_state_reads(
        &self,
        include: impl Fn(&WorkloadInstance) -> bool,
    ) -> Vec<state_sweep::StateRead> {
        self.supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| {
                instance.state == ContainerState::Running
                    && (!instance.is_job
                        || self
                            .recorded_jobs
                            .get(&instance.id.0)
                            .is_some_and(|job| job.phase == super::jobs::JobPhase::Launching))
                    && include(instance)
            })
            .map(|instance| state_sweep::StateRead {
                id: instance.id.clone(),
                incarnation: state_sweep::Incarnation::of(instance),
                is_job: instance.is_job,
            })
            .collect()
    }

    /// Start a sweep of every running app's and job's runtime state, unless
    /// the last one hasn't reported yet. The reads run off the loop; the
    /// sweep's `select!` branch applies what they saw.
    fn begin_state_sweep(&mut self) {
        if !self.state_sweeps.is_empty() {
            return;
        }
        let reads = self.plan_state_reads(|_| true);
        if reads.is_empty() {
            return;
        }
        let grill = self.supervisor.grill().clone();
        self.state_sweeps
            .spawn(state_sweep::sweep_states(grill, reads));
    }

    /// Apply a finished sweep: every app or job it saw exit goes through
    /// the restart or job-outcome path, if it's still the incarnation the
    /// sweep read and still Running.
    async fn apply_state_sweep(
        &mut self,
        sweep: Result<state_sweep::StateSweep, tokio::task::JoinError>,
    ) {
        let sweep = match sweep {
            Ok(sweep) => sweep,
            Err(error) => {
                eprintln!("bun: runtime state sweep failed: {error}");
                return;
            }
        };
        for (read, observed) in sweep.observations {
            let state_sweep::Observed::Exited { exit_code } = observed else {
                continue;
            };
            let current = self
                .supervisor
                .get_instance(&read.id)
                .is_some_and(|instance| {
                    instance.state == ContainerState::Running
                        && state_sweep::Incarnation::of(instance) == read.incarnation
                });
            if !current {
                continue;
            }
            if read.is_job {
                self.observe_job_exit(&read.id, exit_code).await;
            } else {
                self.observe_app_exit(&read.id).await;
            }
        }
    }

    /// A running job's process has exited. Record its outcome; on failure,
    /// attempt a restart or mark it Failed if the retry limit is exhausted.
    async fn observe_job_exit(&mut self, id: &InstanceId, exit_code: Option<i32>) {
        let launching = self
            .recorded_jobs
            .get(&id.0)
            .is_some_and(|job| job.phase == super::jobs::JobPhase::Launching);
        if !launching {
            return;
        }
        let phase = match exit_code {
            Some(code) => super::jobs::JobPhase::Exited { code },
            None => super::jobs::JobPhase::Unknown,
        };
        if let Err(error) = self.record_observed_job_exit(id, phase).await {
            eprintln!("bun: job outcome retained as uncertain for {id}: {error}");
            return;
        }

        // Transition Running → Stopping → Stopped
        if let Some(instance) = self.supervisor.get_instance_mut(id) {
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
            return;
        }
        if exit_code == Some(0) {
            // Job completed successfully — stays in Stopped
            if let Some(instance) = self.supervisor.get_instance(id) {
                self.record_event(
                    crate::bun::events::EventKind::JobCompleted,
                    crate::bun::events::EventSeverity::Info,
                    Some(instance.app_name.clone()),
                    Some(instance.namespace.clone()),
                    format!("job {} completed", instance.app_name),
                )
                .await;
            }
            return;
        }

        // Job failed — attempt restart
        // LOOP-INLINE: in-memory lock, no I/O
        match self.supervisor.maybe_restart(id, Instant::now()).await {
            Ok(true) => {
                // Now in Pending — drive_pending_restarts will handle it
                if let Some(instance) = self.supervisor.get_instance(id) {
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
                if let Some(instance) = self.supervisor.get_instance_mut(id)
                    && let Ok(s) = instance.state.transition_to(ContainerState::Failed)
                {
                    instance.state = s;
                    instance.retry_pending = false;
                }
                if let Some(instance) = self.supervisor.get_instance(id) {
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

    /// A running app's process has exited, which a health check catches
    /// only if the app has one. Mark it Stopped and route it through the
    /// restart path.
    async fn observe_app_exit(&mut self, id: &InstanceId) {
        if let Some(instance) = self.supervisor.get_instance_mut(id) {
            instance.retry_pending = true;
            if let Ok(s) = instance.state.transition_to(ContainerState::Stopping) {
                instance.state = s;
            }
            if let Ok(s) = instance.state.transition_to(ContainerState::Stopped) {
                instance.state = s;
            }
        }
        // LOOP-INLINE: in-memory lock, no I/O
        if let Err(BunError::RestartLimitExceeded { .. }) =
            self.supervisor.maybe_restart(id, Instant::now()).await
            && let Some(instance) = self.supervisor.get_instance_mut(id)
            && let Ok(s) = instance.state.transition_to(ContainerState::Failed)
        {
            instance.state = s;
        }
    }

    /// Read every running app's state and apply what the reads saw, inline.
    /// For tests that drive the agent without running its loop.
    #[cfg(test)]
    async fn check_apps(&mut self) {
        let reads = self.plan_state_reads(|instance| !instance.is_job);
        let grill = self.supervisor.grill().clone();
        let sweep = state_sweep::sweep_states(grill, reads).await;
        self.apply_state_sweep(Ok(sweep)).await;
    }

    /// The same for every running job.
    #[cfg(test)]
    async fn check_jobs(&mut self) {
        let reads = self.plan_state_reads(|instance| instance.is_job);
        let grill = self.supervisor.grill().clone();
        let sweep = state_sweep::sweep_states(grill, reads).await;
        self.apply_state_sweep(Ok(sweep)).await;
    }

    /// Re-drive instances that are in Pending state after a restart, and
    /// clean up after failed attempts.
    ///
    /// The runtime work (kill, create, start) runs in per-restart tasks
    /// (`restarts`); this only starts them. Each phase handles at least one
    /// instance per tick, then stops once `PENDING_RESTART_TICK_BUDGET` is
    /// spent or `RESTARTS_IN_FLIGHT_LIMIT` restarts are in flight.
    /// `restart_rotation` remembers where it stopped, so every instance gets
    /// its turn.
    async fn drive_pending_restarts(&mut self) {
        self.begin_restart_cleanups().await;
        self.begin_restart_launches().await;
    }

    /// Partial startup can have changed the runtime even when its call
    /// failed. Keep ownership until cleanup is observed; then apply the same
    /// budget and backoff as any other failed execution.
    async fn begin_restart_cleanups(&mut self) {
        let deadline = tokio::time::Instant::now() + PENDING_RESTART_TICK_BUDGET;
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
                    && !self.restarting(&instance.id)
                    // Deferred, not dropped: it restarts once the restore ends.
                    && !self
                        .volume_maintenance
                        .restoring(&instance.namespace, &instance.app_name)
            })
            .map(|instance| (instance.id.clone(), instance.state))
            .collect();
        let retrying = rotate_after(retrying, self.restart_rotation.cleanup.as_ref(), |entry| {
            &entry.0
        });
        for (index, (id, state)) in retrying.into_iter().enumerate() {
            if index > 0 && tokio::time::Instant::now() >= deadline {
                break;
            }
            if state == ContainerState::Stopped {
                self.restart_rotation.cleanup = Some(id.clone());
                self.retry_restart(&id).await;
                continue;
            }
            if !self.restart_capacity_left() {
                break;
            }
            self.restart_rotation.cleanup = Some(id.clone());
            match self.poll_instance_withdrawal(&id, self.stop_grace).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    eprintln!("bun: failed restart of {id} awaits discovery withdrawal: {error}");
                    continue;
                }
            }
            self.begin_restart_cleanup(id).await;
        }
    }

    /// Start every pending restart the budget allows. Each begins by killing
    /// what's left of the old container, off the loop. Without that, the
    /// same-id create is rejected (ProcessGrill: stale-Running entry) or
    /// fails (runc/apple: container still exists), leaving the instance
    /// wedged in Preparing and the old process leaked.
    async fn begin_restart_launches(&mut self) {
        let deadline = tokio::time::Instant::now() + PENDING_RESTART_TICK_BUDGET;
        let pending_restarts: Vec<(InstanceId, restarts::RestartLaunch)> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| {
                i.state == ContainerState::Pending
                    && i.restart_count > 0
                    && (!i.is_job || !self.job_store_uncertain)
                    && !self.restarting(&i.id)
            })
            .filter_map(|i| {
                i.oci_spec.as_ref().map(|spec| {
                    (
                        i.id.clone(),
                        restarts::RestartLaunch {
                            oci_spec: spec.clone(),
                            app_name: i.app_name.clone(),
                            namespace: i.namespace.clone(),
                            host_port: i.host_port,
                        },
                    )
                })
            })
            .collect();
        let pending_restarts = rotate_after(
            pending_restarts,
            self.restart_rotation.launch.as_ref(),
            |entry| &entry.0,
        );

        for (index, (id, launch)) in pending_restarts.into_iter().enumerate() {
            if index > 0 && tokio::time::Instant::now() >= deadline {
                break;
            }
            if !self.restart_capacity_left() {
                break;
            }
            self.restart_rotation.launch = Some(id.clone());
            match self.poll_instance_withdrawal(&id, self.stop_grace).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    eprintln!("bun: restart of {id} awaits discovery withdrawal: {error}");
                    continue;
                }
            }
            self.begin_restart_launch(id, launch).await;
        }
    }

    /// Move a Stopped instance whose execution failed back to Pending, if
    /// its restart budget and backoff allow.
    async fn retry_restart(&mut self, id: &InstanceId) {
        // LOOP-INLINE: in-memory lock, no I/O
        match self.supervisor.maybe_restart(id, Instant::now()).await {
            Ok(true) => {
                if let Some(instance) = self.supervisor.get_instance(id) {
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
                if let Some(instance) = self.supervisor.get_instance_mut(id)
                    && let Ok(failed) = instance.state.transition_to(ContainerState::Failed)
                {
                    instance.state = failed;
                    instance.retry_pending = false;
                }
                if let Some(instance) = self.supervisor.get_instance(id) {
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

    /// Run one tick's restart work to completion, runtime steps included,
    /// the way the old inline restart did. For tests that drive the agent
    /// without running its loop.
    #[cfg(test)]
    async fn drive_pending_restarts_to_completion(&mut self) {
        self.begin_restart_cleanups().await;
        self.settle_restart_steps().await;
        self.begin_restart_launches().await;
        self.settle_restart_steps().await;
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
        // LOOP-INLINE: in-memory lock, no I/O
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
            // LOOP-INLINE: in-memory lock, no I/O
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
    /// The removal runs in a task ([`off_loop_work`]); `StillRunning` means ask
    /// again.
    async fn retire_test_storage(
        &mut self,
        app_name: &str,
        namespace: &str,
    ) -> Result<(), BunError> {
        let manager = crate::grill::volume::VolumeManager::new(self.volumes_dir.clone());
        let key = off_loop_work::WorkKey::RetireTestStorage {
            namespace: namespace.to_string(),
            app: app_name.to_string(),
        };
        let (namespace, app) = (namespace.to_string(), app_name.to_string());
        let removal = async move {
            tokio::task::spawn_blocking(move || manager.retire_test_storage(&namespace, &app))
                .await
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())
        };
        self.finish_off_loop_work(key, None, removal)
            .await?
            .map_err(|reason| BunError::DeployFailed {
                app_name: app_name.into(),
                reason,
            })
    }

    /// Claim test storage and create managed volumes before launch. The disk
    /// work runs in a task ([`off_loop_work`]); `StillRunning` means ask again,
    /// and a snapshot restore of the app waits until it has finished.
    async fn prepare_storage(
        &mut self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<(), BunError> {
        // A deploy accepted before the restore still can't mount the volume
        // while the restore is swapping it.
        if self.volume_maintenance.restoring(namespace, app_name) {
            return Err(BunError::DeployFailed {
                app_name: app_name.into(),
                reason: format!(
                    "volumes of {namespace}/{app_name} are being restored from a snapshot"
                ),
            });
        }
        let manager = crate::grill::volume::VolumeManager::new(self.volumes_dir.clone());
        let key = off_loop_work::WorkKey::ProvisionStorage {
            namespace: namespace.to_string(),
            app: app_name.to_string(),
            volumes: format!("{:?}", spec.volumes),
        };
        let (namespace, app) = (namespace.to_string(), app_name.to_string());
        let spec = spec.clone();
        let provisioning = async move {
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
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
        };
        self.finish_off_loop_work(key, None, provisioning)
            .await?
            .map_err(|reason| BunError::DeployFailed {
                app_name: app_name.into(),
                reason,
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
            // LOOP-INLINE: in-memory lock, no I/O
            self.supervisor.stop_app(app_name, namespace).await?;
        }
        // A stop wins over a restart in flight: the restart won't touch the
        // runtime again, and the exit wait lets its current step finish.
        let taken_restarts = instances
            .iter()
            .filter_map(|id| self.take_back_from_restart(id))
            .collect();

        Ok(AppStop {
            instances,
            owns_job,
            taken_restarts,
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
        let taken_restarts = stop.taken_restarts.clone();
        async move {
            restarts::settle_all(&taken_restarts, confirmation_timeout).await?;
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
            ..
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

    /// Restore what `finish_app_stop` released for an app whose stopped
    /// replicas are still owned, so a redeploy can publish into it again.
    ///
    /// Leaves a registered service and a stored route untouched: only a
    /// completed stop removes them while the replicas stay owned. The VIP is
    /// derived from the app's name, so the service comes back under the
    /// address it had before the stop, as the cluster catalogue keeps it.
    async fn restore_stopped_routing(
        &mut self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<(), BunError> {
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
        if let Some(port) = spec.port
            && self.service_map.resolve(&service_id).is_none()
        {
            let firewall = spec
                .firewall
                .as_ref()
                .filter(|firewall| !firewall.allow_from.is_empty())
                .map(|firewall| firewall.allow_from.clone());
            self.register_local_service(&service_id, port, firewall)?;
            self.publish_backend_ebpf(&service_id).await?;
            self.sync_firewall_ebpf().await;
        }
        if let Some(ingress) = &spec.ingress {
            self.ingress_configs
                .entry((namespace.to_string(), app_name.to_string()))
                .or_insert_with(|| ingress.clone());
        }
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

    /// Reconcile the perimeter firewall if cluster membership changed. The
    /// `nft` subprocess runs off the loop; until it reports back, the tick
    /// leaves the firewall alone.
    fn reconcile_firewall(&mut self) {
        if !self.perimeter_config.enabled || self.firewall_applying.is_some() {
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
                // A malformed operator CIDR never reaches nft (NET8); the
                // previous ruleset stays in force.
                eprintln!("warning: firewall ruleset generation failed: {e}");
                return;
            }
        };

        self.spawn_perimeter_apply(ruleset, cluster_nodes);
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
            // LOOP-INLINE: fsync'd persist (#351 decision 2); the slow-disk scenario bounds it
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
        // An initialiser has normally exited long before its parent retires,
        // so confirming that is quick. One the runtime can't confirm within
        // the turn fails the retirement, which retries; the kill is
        // idempotent.
        let deadline = self.turn_deadline();
        for child in children {
            tokio::time::timeout_at(
                deadline,
                kill_runtime_instance(
                    self.supervisor.grill(),
                    &child,
                    self.stop_confirmation_timeout,
                ),
            )
            .await
            .map_err(|_| BunError::StopUnconfirmed {
                instance_id: child.clone(),
                reason: "initialiser exit was not confirmed within the turn",
            })??;
            if let Some(remaining) = self.initialisers.get_mut(parent) {
                remaining.remove(&child);
            }
        }
        self.initialisers.remove(parent);
        Ok(())
    }

    /// Retire durable artifacts before allowing the caller to forget an owner.
    ///
    /// The identity directory and adoption record go last, from a task
    /// ([`off_loop_work`]): `Err(BunError::StillRunning)` means that removal
    /// hasn't finished within the turn, and asking again picks it up where
    /// it is without repeating the steps before it.
    async fn retire_instance_artifacts(
        &mut self,
        instance_id: &InstanceId,
    ) -> Result<(), BunError> {
        let key = off_loop_work::WorkKey::RetireArtifacts(instance_id.clone());
        let incarnation = self.incarnation_of(instance_id);
        if !self.off_loop_work.started(&key, incarnation) {
            self.retire_instance_artifacts_up_to_disk(instance_id)
                .await?;
        }
        let identity_dir = self.instance_identity_dir(instance_id);
        let records_dir = self.records_dir.clone();
        let id = instance_id.0.clone();
        #[cfg(test)]
        let stalls = Arc::clone(&self.loop_stalls);
        let cleanup = async move {
            #[cfg(test)]
            stalls.hold(LoopStall::ArtifactCleanup).await;
            tokio::task::spawn_blocking(move || {
                crate::sesame::identity::cleanup_identity_dir(&identity_dir)?;
                if let Some(directory) = records_dir {
                    crate::grill::records::remove_record(&directory, &id)?;
                }
                Ok::<(), std::io::Error>(())
            })
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
        };
        self.finish_off_loop_work(key, incarnation, cleanup)
            .await?
            .map_err(|reason| BunError::RetirementState {
                instance_id: instance_id.clone(),
                reason,
            })?;
        self.forget_retired_egress_owner(instance_id).await?;
        if let Some(instance) = self.supervisor.get_instance_mut(instance_id) {
            instance.identity = None;
            instance.identity_mount = None;
        }
        Ok(())
    }

    /// Retirement up to the disk cleanup: initialisers, routing, producer
    /// release, egress and the network reference.
    async fn retire_instance_artifacts_up_to_disk(
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
            .await
    }

    /// Retire an instance's artifacts outside the loop (startup adoption),
    /// where nothing else is waiting, so a slow disk is simply waited out.
    async fn retire_instance_artifacts_fully(
        &mut self,
        instance_id: &InstanceId,
    ) -> Result<(), BunError> {
        loop {
            match self.retire_instance_artifacts(instance_id).await {
                Err(BunError::StillRunning { .. }) => continue,
                result => return result,
            }
        }
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

    /// Check identity rotation for all instances, and (rate-limited)
    /// provision identities for running instances that don't have one —
    /// a failed CSR at deploy time, or an adopted instance whose
    /// directory predates the per-instance layout, heals here (D9).
    fn check_identity_rotation(&mut self) {
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

        // Only start the signings here: they finish on the loop when their
        // tasks report back, and one already running for an instance is joined.
        for (id, app, ns, is_job) in needs_rotation {
            self.begin_identity_provision(&app, &ns, &id, is_job, None);
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

    /// What the loop knows about every instance, for a status snapshot.
    fn status_entries(&self) -> Vec<status_snapshot::StatusEntry> {
        use status_snapshot::{EvidenceSource, StatusEntry};
        self.supervisor
            .list_instances()
            .into_iter()
            .map(|instance| {
                let recorded_exit =
                    match self.recorded_jobs.get(&instance.id.0).map(|job| &job.phase) {
                        Some(super::jobs::JobPhase::Exited { code }) => Some(Some(*code)),
                        Some(super::jobs::JobPhase::Unknown) => Some(None),
                        _ => None,
                    };
                let evidence = if instance.is_being_created() {
                    EvidenceSource::Creating {
                        exit_code: recorded_exit.flatten(),
                    }
                } else {
                    EvidenceSource::Runtime {
                        recorded_exit,
                        alive: matches!(
                            instance.state,
                            ContainerState::Running
                                | ContainerState::HealthWait
                                | ContainerState::Unhealthy
                        ),
                    }
                };
                StatusEntry {
                    status: InstanceStatus {
                        id: instance.id.0.clone(),
                        app_name: instance.app_name.clone(),
                        namespace: instance.namespace.clone(),
                        state: self.job_state_label(instance),
                        restart_count: instance.restart_count,
                        host_port: instance.host_port,
                        exit_code: None,
                        pid: None,
                        runtime_unknown: false,
                        status_age_ms: None,
                    },
                    evidence,
                }
            })
            .collect()
    }

    /// Publish what the loop knows now, for status readers, and return it.
    fn publish_status(&self) -> Arc<status_snapshot::StatusSnapshot> {
        let snapshot = Arc::new(status_snapshot::StatusSnapshot::new(self.status_entries()));
        self.status_tx.send_replace(Arc::clone(&snapshot));
        snapshot
    }

    /// A reader that answers status requests from the snapshot this agent's
    /// loop publishes, without queueing anything for the loop.
    pub fn status_reader(&self) -> status_snapshot::StatusReader {
        status_snapshot::StatusReader::new(
            self.status_tx.subscribe(),
            self.supervisor.grill().clone(),
        )
    }

    /// Every instance's status, read the way a status request reads it:
    /// published now, then completed with the runtime's evidence under the
    /// shared `STATUS_RUNTIME_READ_TIMEOUT`.
    #[cfg(test)]
    async fn get_status(&self) -> Vec<InstanceStatus> {
        let snapshot = self.publish_status();
        status_snapshot::read_status(self.supervisor.grill(), &snapshot).await
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
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Withdraw local routing and poll request release without blocking the agent loop.
    async fn poll_instance_withdrawal(
        &mut self,
        id: &InstanceId,
        timeout: std::time::Duration,
    ) -> Result<bool, BunError> {
        self.withdraw_instance_backend(id).await?;
        // LOOP-INLINE: in-memory lock, no I/O
        Ok(self
            .drains
            .drain_all(&[crate::wrapper::draining::DrainCommand {
                app_name: String::new(),
                instance_id: id.0.clone(),
                timeout,
            }])
            .await)
    }

    /// Force-kill `id` and confirm its exit from a task, as `key`'s work
    /// (#351, stage 3). A restart in flight gives the instance up first, and
    /// the task lets its runtime step finish before it signals anything.
    /// Ownership stays put until the kill is confirmed: the outer `Err` is
    /// `StillRunning` while it isn't yet, and the inner one says why the
    /// runtime didn't confirm it.
    async fn kill_off_the_loop(
        &mut self,
        key: off_loop_work::WorkKey,
        id: &InstanceId,
    ) -> Result<Result<(), String>, BunError> {
        let incarnation = self.incarnation_of(id);
        let taken = if self.off_loop_work.started(&key, incarnation) {
            None
        } else {
            self.take_back_from_restart(id)
        };
        let runtime_absent = self
            .recorded_jobs
            .get(&id.0)
            .is_some_and(|job| job.runtime_absent);
        let grill = self.supervisor.grill().clone();
        let confirmation = self.stop_confirmation_timeout;
        let target = id.clone();
        let kill = async move {
            if let Some(restart) = taken {
                restart
                    .settle(confirmation)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            if runtime_absent {
                return Ok(());
            }
            kill_runtime_instance(&grill, &target, confirmation)
                .await
                .map_err(|error| error.to_string())
        };
        self.finish_off_loop_work(key, incarnation, kill).await
    }

    /// Kill `id` off the loop and wait until it's confirmed, the way a
    /// caller that keeps asking would. For tests that drive the agent
    /// without its loop.
    #[cfg(test)]
    async fn kill_and_wait_for_exit(&mut self, id: &InstanceId) -> Result<(), BunError> {
        loop {
            match self
                .kill_off_the_loop(off_loop_work::WorkKey::ClearJobRun(id.clone()), id)
                .await
            {
                Err(BunError::StillRunning { .. }) => tokio::task::yield_now().await,
                Err(error) => return Err(error),
                Ok(result) => return result.map_err(|reason| BunError::StopIncomplete { reason }),
            }
        }
    }

    /// Kill a job's previous run before a rerun replaces it. The first
    /// attempt fences the instance (Stopping, no retries, no probes), so
    /// neither the health tick nor a restart touches it while the kill runs
    /// off the loop; the deploy worker asks again until it's confirmed.
    async fn clear_previous_job_run(&mut self, id: &InstanceId) -> Result<(), BunError> {
        let key = off_loop_work::WorkKey::ClearJobRun(id.clone());
        if !self.off_loop_work.started(&key, self.incarnation_of(id)) {
            if let Some(instance) = self.supervisor.get_instance_mut(id) {
                instance.retry_pending = false;
                if instance.state.can_transition_to(ContainerState::Stopping) {
                    instance.state = ContainerState::Stopping;
                }
            }
            self.supervisor.health_checker_mut().unregister(id);
        }
        self.kill_off_the_loop(key, id).await?.map_err(|reason| {
            BunError::JobState(format!("{id}: previous run not cleared: {reason}"))
        })
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
    async fn begin_instance_retirement(
        &mut self,
        id: &InstanceId,
    ) -> Result<Option<restarts::TakenRestart>, BunError> {
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
        Ok(self.take_back_from_restart(id))
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
        // LOOP-INLINE: in-memory lock, no I/O
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
        // A pressure helper stops in a task; this last turn waits for every
        // one, a start still in flight included, for a few seconds at most.
        let pressure = Arc::clone(&self.node_pressure);
        let _ = tokio::time::timeout(SHUTDOWN_PRESSURE_CLEAR, async move {
            pressure.lock().await.clear_all().await;
        })
        .await;
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

        // A restart step still creating or starting a container would race
        // the signals below. Take every instance back and let those finish.
        for restart in self.take_back_all_restarts() {
            // LOOP-INLINE: shutdown's last turn; settle carries its own deadline
            if let Err(error) = restart.settle(self.stop_confirmation_timeout).await {
                eprintln!("bun: shutting down despite a restart in flight: {error}");
            }
        }

        // Ask everything to stop (SIGTERM), wait (up to a grace period, but no
        // longer than needed) for it to exit, then force-kill (SIGKILL) whatever
        // is still running so nothing is orphaned.
        for id in &ids {
            // LOOP-INLINE: shutdown's last turn; nothing is queued behind it
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
            // LOOP-INLINE: shutdown's last turn; nothing is queued behind it
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        for id in &ids {
            if !matches!(
                self.supervisor.grill().state(id).await,
                Ok(ContainerState::Stopped)
            ) {
                // LOOP-INLINE: shutdown's last turn; nothing is queued behind it
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
mod tests;
