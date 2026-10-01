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
mod commands;
pub use commands::{AgentCommand, ApplyEvent, FaultClearance};
mod job_runs;
mod networking;
mod routing;
use job_runs::ScheduledJob;
mod health_checks;
mod launch;
mod records;
use health_checks::wait_instance_healthy;
use restarts::RestartRotation;

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
use app_stop::{PendingStops, StopPurpose};
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
