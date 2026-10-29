# Deployments & Rollouts

Cross-cutting design for Reliaburger's deployment system: rolling deploys with zero downtime, automatic rollback on health check failure, dependency ordering via `run_before`, deploy history and audit trail, blue-green deploys, and built-in autoscaling.

This subsystem is cross-cutting. It doesn't live in a single component; it coordinates across Meat (scheduling), Bun (container lifecycle), Wrapper (connection draining and routing pool updates), Onion (service map backend updates), and Ketchup (deploy event logging). The deploy orchestration loop itself runs on the leader node as part of the Meat scheduler.

> **Implementation status (0.2.1, #692).** The detailed `DeployState` below is
> the library state machine and Raft schema, not the path the current binary
> persists for cluster rollouts. The shipped cluster path commits desired
> `AppSpec`s, schedules placements and lets each Bun reconcile its local share;
> Bun performs the actual local rolling replacement, bounded by the app's
> `[deploy]` table *per node*.
>
> The leader releases a spec change one node at a time (decision D10-C): its
> `/v1/placements/{node}` answer hands the new spec to the first node, by node
> id, that hasn't converged on it, and holds every later node on the app's last
> fully rolled-out spec (`held = true`) until that node reports its share
> healthy on the new one. Progress comes from the nodes' state reports (each
> reconciler's converged version, `StateReport::rollouts`, plus instance
> health), never from leader-held state, so a new leader resumes where the old
> one stopped. The council holds only `DesiredState::rollouts`: per app, the
> last ten distinct specs that rolled out healthy everywhere
> (`RolloutReleased`) and the spec whose rollout failed (`RolloutFailed`,
> decision D11-B). A node whose rollout aborts posts `/v1/rollouts/failed` to
> the leader; the leader stops releasing that spec until a different one is
> applied. Full leader-coordinated rollouts, with one cluster-wide surge and
> unavailable budget, remain the target described below.
>
> We expose each node's real work through a separate `DeployOperation`: a
> stable ID, target set, current phase/target, timestamps and explicit terminal
> outcome. It is node-local and bounded to fifty terminal records.
> `/v1/deploys/active` and `/v1/deploys/operations` serve it. Each node also
> keeps its last fifty `DeployHistoryEntry` records per app. Claims later in
> this document about every transition living in Raft describe the intended
> converged design, not current production behaviour.

---

## 1. Overview

When an App's image changes (via `git push` through Lettuce or `relish deploy` via CLI), Reliaburger performs a rolling deployment. The system replaces instances one at a time (or in batches governed by `max_surge`), ensuring that at every step in the process, at least the configured number of healthy replicas are serving traffic. The sequence for each instance is:

1. Start a new instance with the new image version on the target node (using a new dynamically allocated host port).
2. Wait for the health check to pass within `health_timeout`.
3. Add the new instance to Wrapper's routing pool and Onion's service map.
4. Remove the old instance from Wrapper's routing pool and Onion's service map.
5. Wait for in-flight requests to drain (up to `drain_timeout`).
6. Stop the old instance and release its host port.

Because host ports are dynamically allocated, old and new versions coexist on the same node without conflict during the transition. There's never a moment when traffic has nowhere to go.

If any new instance fails its health check, the deploy actively reverts all already-upgraded instances (the default) or, when `auto_rollback = false`, halts and leaves the mixed old/new state for the operator to resolve. The system maintains a per-app deploy history accessible via `relish history`, the Brioche UI, and structured Ketchup log events.

Dependency ordering via `run_before` ensures that prerequisite jobs (e.g., database migrations) run to completion before the app's rolling deploy begins. Autoscaling adjusts replica counts at runtime based on CPU or memory metrics, interacting correctly with the GitOps `replicas` field via runtime overrides stored in Raft.

---

## 2. Dependencies

| Component | Role in Deploy System |
|---|---|
| **Meat** (scheduler, leader) | Orchestrates the deploy state machine. Makes placement decisions for new instances. Coordinates the rolling sequence across nodes. Enforces namespace quotas at scheduling time. |
| **Bun** (node agent) | Starts and stops containers on each node. Runs health checks against new instances. Reports instance state back to the leader via the reporting tree. Updates the eBPF service map (Onion) on the local node. |
| **Wrapper** (ingress proxy) | Maintains the routing pool for external traffic. Adds new instances and removes old instances from the pool during transitions. Implements connection draining (stops sending new requests to draining instances, waits for in-flight requests to complete). |
| **Onion** (service discovery) | Updates the eBPF service map on every node so that internal service-to-service traffic routes to the correct backends. Map updates are atomic and take microseconds. |
| **Ketchup** (log collector) | Records structured deploy events (start, step completion, health check pass/fail, drain start/complete, rollback trigger, completion) for audit trail and post-incident analysis. |
| **Lettuce** (GitOps engine) | Triggers deploys when git changes are detected. Treats the `replicas` field as the base count, allowing autoscaler runtime overrides without conflict. |
| **Brioche** (web UI) | Displays deploy history, live deploy progress, and provides one-click rollback. |
| **Mustard** (gossip) | Propagates node health and instance state changes across the cluster. Used by the leader to detect node failures mid-deploy. |
| **Pickle** (image registry) | Serves container images to nodes during deploy. The new image must be available on the target node (pulled or already cached) before the instance can start. |

---

## 3. Architecture

### 3.1 Deploy State Machine

The deploy lifecycle is modeled as an explicit state machine. Each deploy has exactly one active state at any time.

```
                          ┌──────────────────────────────────┐
                          │                                  │
                          ▼                                  │
┌─────────┐    ┌──────────────────┐    ┌──────────────┐     │
│ Pending  │───▶│ RunningPreDeps   │───▶│   Rolling    │     │
└─────────┘    └──────────────────┘    └──────┬───────┘     │
     │                   │                     │             │
     │              dep failure           per-step:          │
     │                   │          ┌──────────┼──────────┐  │
     │                   ▼          │          │          │  │
     │            ┌────────────┐    │  ┌───────▼──────┐   │  │
     │            │   Failed   │    │  │  Starting    │   │  │
     │            │ (dep)      │    │  │  (new inst)  │   │  │
     │            └────────────┘    │  └───────┬──────┘   │  │
     │                              │          │          │  │
     │                              │  ┌───────▼──────┐   │  │
     │                              │  │ HealthWait   │   │  │
     │                              │  │ (checking)   │   │  │
     │                              │  └───┬──────┬───┘   │  │
     │                              │      │      │       │  │
     │                              │   pass │   fail │    │  │
     │                              │      │      │       │  │
     │                              │      │  ┌───▼────┐  │  │
     │                              │      │  │RollFail│──┼──┘
     │                              │      │  └───┬────┘  │  auto_rollback=false
     │                              │      │      │       │  → Halted
     │                              │      │      │       │
     │                              │      │      │ auto_rollback=true
     │                              │      │      ▼       │
     │                              │      │  ┌────────┐  │
     │                              │      │  │Reverting│  │
     │                              │      │  └───┬────┘  │
     │                              │      │      │       │
     │                              │      │      ▼       │
     │                              │      │  ┌────────┐  │
     │                              │      │  │RolledBk│  │
     │                              │      │  └────────┘  │
     │                              │  ┌───▼──────┐       │
     │                              │  │ Draining  │       │
     │                              │  │ (old inst)│       │
     │                              │  └───┬──────┘       │
     │                              │      │              │
     │                              │  ┌───▼──────┐       │
     │                              │  │ Stopped   │       │
     │                              │  │ (old inst)│       │
     │                              │  └──────────┘       │
     │                              │                     │
     │                              │  all steps done     │
     │                              └─────────┬───────────┘
     │                                        │
     │                                        ▼
     │                               ┌─────────────┐
     │                               │  Completed   │
     │                               └─────────────┘
     │
     ▼
┌─────────┐
│Cancelled │
└─────────┘
```

**State transitions:**

| From | To | Trigger |
|---|---|---|
| `Pending` | `RunningPreDeps` | Deploy accepted; pre-dependency jobs (run_before) are dispatched |
| `Pending` | `Cancelled` | Operator cancels before deps start |
| `RunningPreDeps` | `Rolling` | All dependency jobs completed successfully |
| `RunningPreDeps` | `Failed` | A dependency job failed (exhausted retries) |
| `Rolling` | `Rolling` (next step) | Current step completed (new instance healthy, old instance drained and stopped) |
| `Rolling` | `Halted` | Health check failure with `auto_rollback = false`; already-upgraded instances remain on new version, remaining instances stay on old version |
| `Rolling` | `Reverting` | Health check failure with `auto_rollback = true` |
| `Rolling` | `Completed` | All steps finished successfully |
| `Reverting` | `RolledBack` | All already-upgraded instances reverted to previous version |
| `Rolling` | `Halted` | Node failure mid-deploy (operator must intervene) |

### 3.2 Leader-Node Coordination

The deploy orchestration loop runs on the leader node inside Meat. For each rollout step, the leader:

1. Sends a `StartInstance` command to the target node's Bun agent (via the reporting tree).
2. Bun starts the new container, allocates a host port, and begins health checking.
3. Bun reports instance state transitions back to the leader: `Starting` -> `HealthChecking` -> `Healthy` (or `Unhealthy`).
4. On `Healthy`, the leader sends `UpdateRouting` commands:
   - To all Wrapper instances: add the new backend, remove the old backend.
   - To all Bun agents: update the local Onion eBPF service map.
5. The leader sends a `DrainAndStop` command to the node running the old instance.
6. Bun on that node signals the old container (SIGTERM), waits for `drain_timeout`, then force-kills if necessary.
7. Bun reports `Stopped` for the old instance.
8. The leader advances to the next step.

All communication uses the existing reporting tree and Raft log. Deploy progress isn't persisted in Raft: Raft holds only each app's rollout targets (the last specs that rolled out everywhere) and its `RolloutFailed` mark. A new leader reads each node's converged version and health from its reports and carries on from there (see Section 7: Failure Modes).

### 3.3 Wrapper Routing Pool Updates During Transition

Wrapper maintains a routing pool per ingress route (keyed by `host` + `path`). During a rolling deploy, the pool temporarily contains backends running both the old and new versions. The sequence for external traffic routing is:

1. **Before deploy:** Pool contains `[web-1(v1, nodeA:3001), web-2(v1, nodeB:3002), web-3(v1, nodeC:3003)]`.
2. **Step 1 transition:** Pool becomes `[web-4(v2, nodeA:3004), web-2(v1, nodeB:3002), web-3(v1, nodeC:3003)]` after web-4 is healthy and web-1 starts draining.
3. **During drain:** Wrapper stops sending NEW requests to web-1 but allows in-flight requests to complete. Web-1 remains in the pool in `draining` state until all connections close or `drain_timeout` expires.
4. **After drain:** web-1 is fully removed from the pool and stopped.

Wrapper receives routing pool updates via gossip (Mustard). The propagation delay is bounded by the gossip protocol's convergence time (typically under 2 seconds for health state changes). The leader doesn't advance to the drain phase until it has confirmation (via the reporting tree) that routing updates have propagated.

### 3.4 Connection Draining Protocol

Connection draining ensures that in-flight requests complete before an instance is stopped. The protocol is:

```
                    Leader                         Bun (old instance node)                  Wrapper (all nodes)
                      │                                     │                                      │
                      │──── DrainInstance(web-1) ──────────▶│                                      │
                      │                                     │                                      │
                      │                                     │──── mark web-1 as draining ─────────▶│
                      │                                     │                                      │
                      │                                     │     Wrapper stops sending new         │
                      │                                     │     requests to web-1.                │
                      │                                     │     Existing connections continue.    │
                      │                                     │                                      │
                      │                                     │◀─── in-flight count updates ─────────│
                      │                                     │                                      │
                      │                                     │     [wait until in-flight == 0        │
                      │                                     │      OR drain_timeout expires]        │
                      │                                     │                                      │
                      │                                     │──── SIGTERM to container ────────────▶│ (container)
                      │                                     │                                      │
                      │                                     │     [wait grace_period (10s default)] │
                      │                                     │                                      │
                      │                                     │──── SIGKILL if still running ────────▶│ (container)
                      │                                     │                                      │
                      │◀─── InstanceStopped(web-1) ────────│                                      │
                      │                                     │                                      │
```

For internal (service-to-service) traffic via Onion, the eBPF service map is updated atomically. Bun removes the draining instance's entry from the kernel BPF hash map on every node. Because the map update is atomic and takes microseconds, new `connect()` calls immediately stop targeting the draining instance. Existing TCP connections (already established) continue until they close naturally or the container is stopped.

### 3.5 What Other Nodes See During a Rollout

Other nodes route by the leader's endpoint catalogue, which the leader builds from node state reports (`build_endpoint_catalog` in `src/cluster/orchestrate.rs`). A backend is healthy there only when its node reports the instance `Healthy`, and a node reports `Healthy` only for a `Running` instance.

A rolling replacement stays `Starting` in the supervisor until `finalise_rolling_deploy`, which runs after every old instance has drained and stopped. Supervision (health checks, crash restarts) takes it over only then, so an aborted rollout can tear replacements down without a restart racing it. But it serves traffic from the moment `start_replacement` publishes its backend, after its health wait. So the state report gives an instance that is still `Starting` or `HealthWait` and has a healthy backend in the node's service map the state `Running` (`reported_state` in `src/bun/agent/status.rs`). Since the worker publishes the replacement before it begins retiring an old instance, no report shows the old instance `Stopping` without a healthy replacement beside it, and the catalogue moves from "old healthy" to "new healthy, old unhealthy" with no step in between.

Before this (#1353), a one-replica app rolled with `max_surge = 1`, `max_unavailable = 0` had no healthy backend in the catalogue from the first report after the old instance began retiring until the first report after finalisation, three to six seconds on a laptop cluster, and every other node's connect hook refused the service with `EPERM`. `one_replica_rollout_keeps_a_healthy_backend_on_every_node` in `tests/cluster_failover.rs` covers it.

A retiring old instance also has to keep running until no node routes to it (#1355). Its drain waits only for this node's ingress connections; service-to-service callers on other nodes connect to the backend directly and drop it only when their next view arrives. So `retire_old_instance` in `src/bun/agent/deploy_worker.rs` runs two waits together before the stop signal: the ingress drain, and `await_remote_withdrawal`, which asks the leader the producer-release question (`confirm_producer_release`) every 500 ms through the `AwaitRemoteWithdrawal` deploy op. The order is:

1. Publish the replacement, then begin the old instance's retirement (local withdrawal, `Stopping`, restarts fenced).
2. Ask the leader to retire the old execution. The leader fences it, drops it from the catalogue and records a withdrawal every consumer must acknowledge. It answers `202` until every receipt is in, then confirms.
3. Wait until the leader confirms and the ingress drain finishes, or `PRODUCER_RELEASE_PATIENCE` (5 s) passes.
4. Stop the instance (pre-stop, `SIGTERM`, grace, `SIGKILL`).
5. Release its addresses once the leader confirms again, which is immediate when step 3 got a confirmation; otherwise the agent loop finishes the release later (deferred retirement).

A node that never sends its receipt costs the rollout at most the 5 s patience, after which the instance stops anyway; the leader discharges that node when its view lease lapses, and step 5 still keeps the addresses until the leader confirms. An unreachable leader or uncertain discovery ownership gets the same bounded wait, logged. Without a cluster or durable discovery there is nothing to ask and no wait. No wire or state format changes: step 2 is the release request the producer always sent, only earlier, and re-fencing an execution is idempotent. `a_rolling_retirement_stops_the_old_instance_only_after_other_nodes_drop_it` in the agent tests checks that every release request before the confirmation arrives while the old instance is still running. The in-process cluster test fixture can't cover it: it runs plain HTTP without durable discovery, and the producer release refuses both.

---

## 4. Data Structures

These structures are the original design. What ships keeps less: `DeployState` and its phases live in `src/meat/deploy_types.rs` as a tested model the agent doesn't drive, deploy history is a capped per-node list, and Raft holds only `AppRollouts` (rollout targets and `RolloutFailed`). Field types use standard Rust conventions.

```rust
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// A request to begin a new deployment, submitted via CLI, API, or GitOps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployRequest {
    /// Unique deploy ID, monotonically increasing per app.
    pub deploy_id: u64,
    /// The target app name (e.g., "web", "api").
    pub app_name: String,
    /// The namespace this app belongs to.
    pub namespace: String,
    /// The new image reference (e.g., "pickle://api:v1.4.3" or "docker.io/myapp:v2").
    pub new_image: ImageRef,
    /// The previous image reference (for rollback).
    pub previous_image: ImageRef,
    /// Deploy configuration (strategy, timeouts, rollback behaviour).
    pub config: DeployConfig,
    /// Who initiated this deploy.
    pub initiated_by: DeployActor,
    /// Timestamp when the deploy was requested.
    pub requested_at: chrono::DateTime<chrono::Utc>,
    /// Pre-deploy dependency jobs that must complete before rolling begins.
    pub pre_dependencies: Vec<DependencyRef>,
    /// The desired replica count (may differ from current if autoscaler adjusted).
    pub desired_replicas: u32,
}

/// Actor who initiated the deploy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeployActor {
    User { username: String, source: String },      // "alice@myorg via relish deploy"
    GitOps { commit_sha: String, repo: String },    // Lettuce-triggered
    Autoscaler,                                      // replica count change
    Rollback { from_deploy_id: u64 },               // explicit rollback
}

/// Full deploy state (designed for Raft; not persisted today).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployState {
    pub request: DeployRequest,
    pub phase: DeployPhase,
    /// Ordered list of rollout steps (one per instance to replace).
    pub steps: Vec<RolloutStep>,
    /// Index of the currently executing step (0-based).
    pub current_step: usize,
    /// Dependency job statuses (populated during RunningPreDeps phase).
    pub dependency_status: Vec<DependencyJobStatus>,
    /// If a rollback is in progress, tracks revert steps.
    pub rollback_steps: Vec<RollbackStep>,
    /// Timestamps for phase transitions.
    pub phase_transitions: Vec<PhaseTransition>,
    /// Terminal result, set when deploy reaches a final state.
    pub result: Option<DeployResult>,
}

/// The deploy phase (state machine states).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeployPhase {
    /// Deploy accepted, waiting to start.
    Pending,
    /// Pre-dependency jobs (run_before) are executing.
    RunningPreDeps,
    /// Rolling update in progress.
    Rolling,
    /// Health check failed, auto_rollback=false; deploy paused with mixed versions.
    Halted { failed_step: usize, error: String },
    /// Health check failed, auto_rollback=true; actively reverting upgraded instances.
    Reverting,
    /// All instances reverted to previous version.
    RolledBack,
    /// All steps completed successfully.
    Completed,
    /// Pre-dependency job failed.
    Failed { error: String },
    /// Operator cancelled the deploy.
    Cancelled { reason: String },
}

/// A single step in the rolling update: replace one old instance with one new instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolloutStep {
    /// Which node this step targets.
    pub node_id: NodeId,
    /// The old instance being replaced.
    pub old_instance: InstanceId,
    /// The new instance being started.
    pub new_instance: InstanceId,
    /// Host port allocated for the new instance.
    pub new_host_port: Option<u16>,
    /// Current step phase.
    pub step_phase: StepPhase,
    /// Timestamps for each sub-phase transition.
    pub timestamps: StepTimestamps,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepPhase {
    /// Waiting for previous step to complete.
    Pending,
    /// New instance is starting (image pulled, container created).
    Starting,
    /// New instance is running, health checks in progress.
    HealthChecking,
    /// Health check passed; routing update in progress.
    RoutingUpdate,
    /// Old instance removed from routing pool; draining in-flight connections.
    Draining,
    /// Old instance stopped and cleaned up.
    Completed,
    /// Health check failed for this step.
    Failed { error: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepTimestamps {
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub healthy_at: Option<chrono::DateTime<chrono::Utc>>,
    pub routing_updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub drain_started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub drain_completed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub stopped_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Tracks connection draining state for a single instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrainState {
    pub instance_id: InstanceId,
    pub node_id: NodeId,
    /// When draining was initiated.
    pub drain_started_at: chrono::DateTime<chrono::Utc>,
    /// Configured drain timeout.
    pub drain_timeout: Duration,
    /// Number of in-flight connections (reported by Wrapper).
    pub in_flight_connections: u32,
    /// Whether the drain completed cleanly (all connections closed)
    /// or was forced (timeout expired with connections remaining).
    pub outcome: Option<DrainOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DrainOutcome {
    /// All in-flight connections completed before timeout.
    Clean,
    /// Timeout expired; container was force-stopped with connections remaining.
    ForcedTimeout { remaining_connections: u32 },
}

/// Decision made when a health check fails during rollout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackDecision {
    pub deploy_id: u64,
    pub failed_step: usize,
    pub failure_reason: String,
    pub action: RollbackAction,
    pub decided_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RollbackAction {
    /// Deploy halts. Already-upgraded instances stay on new version.
    /// Remaining instances stay on old version. Operator must intervene.
    Halt,
    /// System actively reverts all upgraded instances to previous version.
    ActiveRevert,
}

/// A step in the rollback process (reverting an already-upgraded instance).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackStep {
    pub node_id: NodeId,
    /// The instance running the new (failed) version.
    pub new_instance: InstanceId,
    /// The replacement instance running the old (good) version.
    pub reverted_instance: InstanceId,
    pub step_phase: StepPhase,
    pub timestamps: StepTimestamps,
}

/// Per-app deploy history entry, stored in Raft and queryable via CLI/API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployHistory {
    pub app_name: String,
    pub namespace: String,
    /// Ordered list of deploys, most recent first.
    pub entries: Vec<DeployHistoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployHistoryEntry {
    pub deploy_id: u64,
    pub from_image: ImageRef,
    pub to_image: ImageRef,
    pub initiated_by: DeployActor,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub result: DeployResult,
    pub duration: Duration,
    /// Number of instances that were rolled.
    pub instances_rolled: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeployResult {
    Completed,
    RolledBack { reason: String },
    Halted { reason: String },
    Failed { reason: String },
    Cancelled { reason: String },
}

/// Dependency graph for run_before ordering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyGraph {
    /// Edges: job_name -> list of apps that depend on this job completing.
    /// The job must complete before any of the listed apps begin rolling.
    pub edges: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyRef {
    /// The job name (e.g., "db-migrate").
    pub job_name: String,
    /// The job's image (must match the deploy's new image for consistency).
    pub image: ImageRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyJobStatus {
    pub job_name: String,
    pub state: DependencyJobState,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub exit_code: Option<i32>,
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DependencyJobState {
    Pending,
    Running,
    Succeeded,
    Failed { error: String },
}

/// Deploy configuration, parsed from the [app.<name>.deploy] TOML section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployConfig {
    /// Deploy strategy. Currently only "rolling" is supported; "blue-green" planned.
    pub strategy: DeployStrategy,
    /// Maximum number of extra instances during rolling update.
    /// Default: 1. Determines how many instances are replaced in parallel.
    pub max_surge: u32,
    /// How long to wait for in-flight connections to drain before force-stopping.
    /// Default: 30s.
    pub drain_timeout: Duration,
    /// How long to wait for a new instance's health check to pass.
    /// Default: 60s.
    pub health_timeout: Duration,
    /// Whether to actively revert on health check failure (true) or just halt (false).
    /// Default: true.
    pub auto_rollback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeployStrategy {
    Rolling,
    BlueGreen,
}

impl Default for DeployConfig {
    fn default() -> Self {
        Self {
            strategy: DeployStrategy::Rolling,
            max_surge: 1,
            drain_timeout: Duration::from_secs(30),
            health_timeout: Duration::from_secs(60),
            auto_rollback: true,
        }
    }
}

/// Autoscale configuration, parsed from the [app.<name>.autoscale] TOML section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoscaleConfig {
    /// The metric to scale on.
    pub metric: AutoscaleMetric,
    /// Per-replica request in the collector series' unit (percent of one
    /// core for CPU, bytes for memory).
    pub request: f64,
    /// Target utilisation of the request as a fraction (0.70 for "70%").
    pub target: f64,
    /// Minimum number of replicas (autoscaler will never scale below this).
    pub min: u32,
    /// Maximum number of replicas (autoscaler will never scale above this).
    pub max: u32,
    /// Evaluation window: how long the metric must exceed the target before scaling.
    /// Default: 5 minutes.
    pub evaluation_window: Duration,
    /// Cooldown period after a scale event before the next evaluation.
    /// Default: 3 minutes.
    pub cooldown: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutoscaleMetric {
    Cpu,
    Memory,
}

/// Runtime autoscaler state, stored in Raft as a runtime override.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoscaleState {
    pub app_name: String,
    pub namespace: String,
    /// The base replica count from the TOML config / git.
    pub base_replicas: u32,
    /// The current runtime replica count (autoscaler-adjusted).
    pub current_replicas: u32,
    /// Last scale event.
    pub last_scale_event: Option<ScaleEvent>,
    /// Timestamp of last metric evaluation.
    pub last_evaluated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScaleEvent {
    pub from_replicas: u32,
    pub to_replicas: u32,
    pub reason: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}
```

---

## 5. Operations

### 5.1 Rolling Deploy Sequence

The rolling deploy is the primary deploy strategy. Given an app with `replicas = 3` and `max_surge = 1`, upgrading from v1 to v2:

```
Phase           Node A              Node B              Node C              Routing Pool
──────────────  ──────────────────  ──────────────────  ──────────────────  ──────────────────────────
Initial         web-1 (v1, :3001)   web-2 (v1, :3002)   web-3 (v1, :3003)   [web-1, web-2, web-3]

Step 1: Start   web-1 (v1, :3001)   web-2 (v1, :3002)   web-3 (v1, :3003)   [web-1, web-2, web-3]
  new on A      web-4 (v2, :3004)                                            (web-4 starting...)
                  ↑ health check

Step 1: Swap    web-1 (v1, :3001)   web-2 (v1, :3002)   web-3 (v1, :3003)   [web-4, web-2, web-3]
                web-4 (v2, :3004)                                            (web-1 draining...)
                  ✓ healthy

Step 1: Drain   web-4 (v2, :3004)   web-2 (v1, :3002)   web-3 (v1, :3003)   [web-4, web-2, web-3]
  + stop old    web-1 stopped

Step 2: Start   web-4 (v2, :3004)   web-2 (v1, :3002)   web-3 (v1, :3003)   [web-4, web-2, web-3]
  new on B                          web-5 (v2, :3005)                        (web-5 starting...)
                                      ↑ health check

Step 2: Swap    web-4 (v2, :3004)   web-2 (v1, :3002)   web-3 (v1, :3003)   [web-4, web-5, web-3]
                                    web-5 (v2, :3005)                        (web-2 draining...)
                                      ✓ healthy

Step 2: Drain   web-4 (v2, :3004)   web-5 (v2, :3005)   web-3 (v1, :3003)   [web-4, web-5, web-3]
  + stop old                        web-2 stopped

Step 3: Start   web-4 (v2, :3004)   web-5 (v2, :3005)   web-3 (v1, :3003)   [web-4, web-5, web-3]
  new on C                                               web-6 (v2, :3006)   (web-6 starting...)
                                                           ↑ health check

Step 3: Swap    web-4 (v2, :3004)   web-5 (v2, :3005)   web-3 (v1, :3003)   [web-4, web-5, web-6]
                                                         web-6 (v2, :3006)   (web-3 draining...)
                                                           ✓ healthy

Step 3: Drain   web-4 (v2, :3004)   web-5 (v2, :3005)   web-6 (v2, :3006)   [web-4, web-5, web-6]
  + stop old                                             web-3 stopped

Final           web-4 (v2, :3004)   web-5 (v2, :3005)   web-6 (v2, :3006)   [web-4, web-5, web-6]
```

With `max_surge = 2`, steps 1 and 2 execute in parallel (two new instances starting simultaneously), reducing total deploy time at the cost of temporarily using more resources.

**Per-step algorithm (pseudocode):**

```rust
async fn execute_rollout_step(step: &mut RolloutStep, config: &DeployConfig) -> Result<()> {
    // 1. Start new instance
    step.step_phase = StepPhase::Starting;
    let port = bun_client.start_instance(step.node_id, &step.new_instance).await?;
    step.new_host_port = Some(port);
    step.timestamps.started_at = Some(Utc::now());

    // 2. Wait for health check
    step.step_phase = StepPhase::HealthChecking;
    let healthy = tokio::time::timeout(
        config.health_timeout,
        bun_client.await_healthy(step.node_id, &step.new_instance),
    ).await;

    match healthy {
        Ok(Ok(())) => {
            step.timestamps.healthy_at = Some(Utc::now());
        }
        Ok(Err(e)) => {
            step.step_phase = StepPhase::Failed { error: e.to_string() };
            return Err(e);
        }
        Err(_timeout) => {
            step.step_phase = StepPhase::Failed {
                error: format!("health check timeout after {:?}", config.health_timeout),
            };
            return Err(DeployError::HealthTimeout);
        }
    }

    // 3. Update routing: add new, remove old
    step.step_phase = StepPhase::RoutingUpdate;
    wrapper_client.add_backend(&step.new_instance, step.node_id, port).await?;
    wrapper_client.remove_backend(&step.old_instance).await?;
    onion_client.update_service_map_add(&step.new_instance, step.node_id, port).await?;
    onion_client.update_service_map_remove(&step.old_instance).await?;
    step.timestamps.routing_updated_at = Some(Utc::now());

    // 4. Drain old instance
    step.step_phase = StepPhase::Draining;
    step.timestamps.drain_started_at = Some(Utc::now());
    let drain_result = tokio::time::timeout(
        config.drain_timeout,
        wrapper_client.await_drain_complete(&step.old_instance),
    ).await;
    step.timestamps.drain_completed_at = Some(Utc::now());

    // 5. Stop old instance (regardless of drain outcome)
    bun_client.stop_instance(step.node_id, &step.old_instance).await?;
    step.timestamps.stopped_at = Some(Utc::now());
    step.step_phase = StepPhase::Completed;

    Ok(())
}
```

### 5.2 Automatic Rollback

Two modes, determined by the `auto_rollback` config field:

**Mode 1: Halt (`auto_rollback = false`)**

When a new instance fails its health check, the deploy halts immediately. The system state becomes a mix of old and new versions:

- Instances that already upgraded successfully remain on the new version (they passed health checks and are serving traffic).
- Instances that haven't yet been rolled remain on the old version.
- The failed instance is stopped (it never entered the routing pool).

The operator can inspect the situation via `relish status`, which clearly shows the partial rollout state (e.g., "web: 1/3 instances on v2, 2/3 on v1, deploy halted"). The operator then decides whether to fix the issue and continue or manually roll back. (The `relish deploy --continue` resume flow and a dedicated `relish rollback` command are **planned — not yet implemented**; rollback today is a fresh deploy of the previous spec version.)

**Mode 2: Active Revert (default, `auto_rollback = true`)**

When a new instance fails its health check, the system automatically reverts all already-upgraded instances back to the previous version. The revert uses the same rolling mechanism (start old version, health check, swap routing, drain new version, stop new version) but in reverse. After revert completes, the entire app is back on the old version with no manual intervention required.

**Shipped (0.2.1, #692):** each node reverts its own share. If no old instance had retired yet, the node stops the replacements and the old instances carry on. If some had, it starts previous-spec instances for the ordinals no old instance holds, under the same `max_surge`/`max_unavailable` budget, and retires a healthy replacement only once one of them is healthy. It never ends below `min(healthy new + remaining old, replicas)`: if the previous spec can't come back, the healthy replacements keep serving and the result is `Halted` with the reason. Before #692 the abort killed healthy replacements and never restarted retired old instances, so a stop-first volume app with one replica could end with none. In a cluster, the node then reports the failure to the leader, which commits `RolloutFailed` for that spec (D11-B). The leader doesn't revert the desired spec in Raft, because GitOps would only apply it again; it stops releasing the failed spec to further nodes, and `relish status`, `relish wtf` and the desired-apps API show where and why. Re-applying the identical spec keeps the mark, and `relish apply` warns that it isn't retried; any other spec clears it, a scale change included (#929).

```
Failure detected at step 2 (auto_rollback = true):
  State: web-4 (v2, healthy), web-5 (v2, FAILED health check), web-3 (v1, healthy)

Revert sequence:
  1. Stop web-5 (never entered routing pool, no drain needed)
  2. Start web-2' (v1) on Node B, wait for health check
  3. Add web-2' to routing pool
  4. Start web-1' (v1) on Node A, wait for health check
  5. Add web-1' to routing pool, remove web-4 from routing pool
  6. Drain web-4, stop web-4

Result: web-1' (v1, Node A), web-2' (v1, Node B), web-3 (v1, Node C)
         All on previous version. Deploy recorded as "rolled back".
```

### 5.3 Deploy History Tracking

There are three shipped histories. Bun's `DeployOperation` store describes the
worker which really ran: it stays visible after the SSE consumer disconnects
and records `unknown` if that worker disappears without a terminal event. The
per-app `DeployHistoryEntry` list on each node describes deploys, retries and
aborts with their specs, capped at fifty per app and lost on restart. The
council's `AppRollouts::released` list holds the last ten distinct specs that
rolled out healthy on every node; cluster rollback reads it, so `relish
rollback` survives every node restarting. None of them should be confused with
the Raft-backed design below, which remains the target architecture.

Every deploy is recorded in the per-app deploy history stored in Raft. The history includes:

- Deploy ID (monotonically increasing per app)
- Image transition (from -> to)
- Who initiated it (user, GitOps commit, autoscaler, rollback)
- Start and completion timestamps
- Duration
- Result (completed, rolled back, halted, failed, cancelled)
- For rollbacks: the reason and which step failed

The history is accessible via:

- `relish history <app>` (CLI)
- Brioche UI deploy tab
- Ketchup structured log events (for external log aggregation)

```
$ relish history web

web deploys:
  #4  2026-02-10 14:32  v1.4.2 -> v1.4.3  completed in 2m15s  (alice@myorg via relish deploy)
  #3  2026-02-08 09:15  v1.4.1 -> v1.4.2  completed in 1m48s  (ci@github, commit a1b2c3d)
  #2  2026-02-07 16:00  v1.4.0 -> v1.4.1  rolled back (health check failure at step 2/3)
  #1  2026-02-01 10:00  (initial deploy)   completed in 45s    (alice@myorg via relish deploy)
```

Rollback re-deploys a previous spec version through the same rolling mechanism, creating a new deploy history entry with `initiated_by: Rollback`. A single `relish rollback web --to v1.4.1` command that targets a named version, and one-click rollback in Brioche, are **planned — not yet implemented**.

### 5.4 Dependency Ordering (`run_before`)

> **Status: implemented.** Before an app deploys, the agent runs every job whose
> `run_before` names that app to completion, gating on a clean (exit 0) finish
> and aborting the deploy if a prerequisite fails or times out. A gating job runs
> exactly once and is skipped in the regular jobs loop. The multi-stage
> `DependencyGraph` and topological cycle detection below remain the target for a
> future iteration; today's gating covers the job→app case (the common migration
> ordering).

Jobs can declare dependencies on Apps using the `run_before` field:

```toml
[job.db-migrate]
image = "myapp:v1.4.2"
command = ["npm", "run", "migrate"]
run_before = ["app.api"]
```

When a deploy includes both the migration job and the API app (e.g., both reference the same new image tag, or both are declared in the same TOML file), the deploy system:

1. Parses the `DependencyGraph` from all jobs with `run_before` fields referencing the target app.
2. Transitions the deploy to `RunningPreDeps` phase.
3. Dispatches all dependency jobs to the Meat scheduler (jobs without inter-dependencies run in parallel).
4. Waits for all dependency jobs to complete successfully.
5. On success: transitions to `Rolling` phase and begins the rolling update.
6. On failure (any dependency job fails after exhausting retries): transitions to `Failed` with a clear error indicating which job failed and why. The app isn't modified.

For multi-stage dependencies (e.g., `build-frontend` -> `build-api` -> `app.api`), the dependency graph is resolved topologically. Cycles are detected at deploy time and rejected with an error.

### 5.5 Connection Draining

When an instance is being replaced, Wrapper coordinates connection draining:

1. **Wrapper removes the instance from active load balancing.** New incoming requests are no longer routed to this instance.
2. **Existing connections continue.** Wrapper tracks the number of in-flight connections to the draining instance.
3. **Wait for completion.** The system waits until either:
   - All in-flight connections complete (clean drain), OR
   - `drain_timeout` expires (forced drain).
4. **Stop:** once the drain is clean or `drain_timeout` expires, Bun stops the instance with its stop hooks (5.8): its `pre_stop` command, then its `stop_signal` (SIGTERM by default), then its `stop_grace` (the `drain_timeout` again by default, 10 s for a stop outside a deploy), then SIGKILL.

For WebSocket connections, the drain timeout applies equally. Long-lived WebSocket connections that exceed the drain timeout are forcefully closed. Applications that need longer drain times can configure `drain_timeout` accordingly.

For internal service-to-service traffic (via Onion), the eBPF service map entry is removed atomically. New `connect()` calls immediately stop targeting the instance. Existing established TCP connections are unaffected by the map removal (they are kernel-level connections that persist independently of the BPF map).

### 5.6 Blue-Green Deploy

> **Status: implemented.** A config that sets `strategy = "blue-green"` now
> dispatches to the agent's `blue_green_redeploy`: the whole green fleet comes up
> and health-checks, one instance at a time, while blue keeps serving. Then every
> green backend is published and blue is drained and retired. A green failure
> tears green down and leaves blue untouched (or halts for inspection when
> `auto_rollback = false`). The separate `meat::blue_green` model was deleted in
> 1e514421, so the agent's path is the only implementation.

Blue-green deployment starts an entirely new set of instances (the "green" set) alongside the existing set (the "blue" set), verifies health on all green instances, then switches all traffic at once:

```
Phase 1: Start green set
  blue:  web-1 (v1, :3001), web-2 (v1, :3002), web-3 (v1, :3003)   <-- serving traffic
  green: web-4 (v2, :3004), web-5 (v2, :3005), web-6 (v2, :3006)   <-- health checking

Phase 2: All green instances healthy — switch traffic
  blue:  web-1 (v1, :3001), web-2 (v1, :3002), web-3 (v1, :3003)   <-- draining
  green: web-4 (v2, :3004), web-5 (v2, :3005), web-6 (v2, :3006)   <-- serving traffic

Phase 3: Drain complete — stop blue set
  green: web-4 (v2, :3004), web-5 (v2, :3005), web-6 (v2, :3006)   <-- serving traffic
```

Blue-green requires `2 * replicas` worth of resources during the transition. The advantage is that the switch is quick (green is published as a whole, then blue drains) and nothing of blue is touched until green is healthy. The disadvantage is the resource overhead.

Blue-green is configured via:

```toml
[app.web.deploy]
strategy = "blue-green"
health_timeout = "120s"     # more time since all instances start at once
drain_timeout = "30s"
auto_rollback = true        # if any green instance fails, abandon the green set
```

### 5.7 Autoscaling

Autoscaling adjusts the runtime replica count based on observed metrics. The autoscaler runs as a periodic evaluation loop on the leader node (inside Meat), separate from the deploy state machine.

**Configuration:**

```toml
[app.web.autoscale]
metric = "cpu"      # or "memory"; anything else fails validation
target = "70%"      # target 70% utilisation of the request
min = 2             # never scale below 2 replicas
max = 10            # never scale above 10 replicas
```

**What "utilisation" means:** average per-replica use divided by the per-replica request, the Kubernetes HPA convention. CPU reads the collector's `process_cpu_percent` (percent of one core) against the `cpu` request (500m = half a core); an app with no CPU request is measured against one whole core, because ProcessGrill and rootless nodes refuse apps that declare `cpu`. Memory reads `process_memory_bytes` against the `memory` request, and fails validation without one. See `design/scheduler-meat.md` §5.4 for the full semantics and limits.

**Evaluation algorithm:**

1. Every 30 seconds, the autoscaler queries Mayo for the average utilisation across all instances of the app over the `evaluation_window` (default: 5 minutes).
2. If the average exceeds `target`, compute the desired replica count: `desired = ceil(current_replicas * (current_metric / target))`.
3. If the average is below `target * 0.8` (80% of target, to avoid flapping), compute the desired replica count using the same formula (this will produce a lower number).
4. Clamp `desired` to `[min, max]`.
5. If `desired != current_replicas` and the cooldown period has elapsed since the last scale event, initiate a scale operation.

**Scale-up** starts new instances using the same placement logic as a deploy (Meat picks nodes, Bun starts containers, health checks run, Wrapper/Onion add to pools). Scale-up doesn't drain or stop any existing instances: a node whose share grows receives the same spec with a higher count, and its agent starts only the added replicas beside the running ones (#346).

**Scale-down** removes excess instances using the drain protocol (remove from routing, wait for drain, stop). Meat keeps the first placements and drops the rest. A node whose share shrinks stops only the replicas whose ordinals left it, and leaves the rest running (#1111).

**Moving replicas** between nodes (a lost node's replicas re-placed, an upgrade walk, a capacity refusal) changes a node's share without changing the version. The agent compares the new share with the one it runs: if nothing but the fixed count and the assigned ordinals changed, and every replica of the old share is running, it retires the departing ordinals first and then starts the arriving ones, so a full node frees the room the new replica needs. Replicas that stay in the share aren't touched. Any other change, a new version included, still rolls every replica (#1111).

**GitOps interaction (Lettuce):**

The `replicas` field in git is the *base* replica count. Autoscaler adjustments are runtime overrides stored in Raft, not in git. Lettuce compares the `replicas` field independently from other fields:

- A change to `replicas` in git resets the runtime override (the operator is explicitly setting a new base).
- Changes to other fields (image, env, resources) trigger rolling deploys without resetting the autoscaler's current count.
- `relish diff` shows autoscaler overrides as expected runtime drift, not configuration drift.

This prevents Lettuce from fighting the autoscaler during traffic spikes, even when unrelated configuration changes are deployed.

---

### 5.8 Stop Hooks

An app can say how its instances want to be stopped (#1121):

```toml
[app.worker]
image = "registry.example.com/worker:2.1"
stop_signal = "SIGQUIT"     # default SIGTERM
stop_grace = "2m"           # 1s to 1h
pre_stop = ["/app/bin/worker", "hand-back-jobs"]
pre_stop_timeout = "20s"    # default 30s, 1s to 1h
```

**Where the policy lives.** `grill::oci::generate_oci_spec` copies the four fields into the instance's `OciSpec` as a `StopPolicy` (`stop`), so it's fixed per instance at launch, recorded with every instance record, runtime intent and process owner record, and survives adoption after a Bun restart. A rolling deploy retires each old instance with the policy that instance was launched with, not the new spec's, because the new spec's pre-stop command may not exist in the old image.

**Who sends the signal.** Each runtime reads the signal from the spec it holds: `ProcessGrill` signals the process group itself or asks the process owner, which reads its launch spec; runc runs `runc kill <id> <SIGNAL>` from the runtime intent; Apple Container runs `container kill --signal <SIGNAL>`, not `container stop`, which would kill the workload after its own 5 s default and cut every longer `stop_grace` short (#1435). Every runtime's `stop` only signals; Bun waits out the grace and sends the kill. Every graceful stop, including a Bun shutdown, therefore sends the app's signal, and `Grill::stop` keeps its one-argument shape.

**Who runs the sequence.** `bun::agent::stop_runtime_instance` runs the four steps, each with its own bound:

1. `pre_stop` through `Grill::exec`, within `pre_stop_timeout`. A non-zero exit, a refused exec or an overrun is logged and the stop carries on. Dropping the exec future ends the command (the owner closes it; the plain process runtime spawns it with `kill_on_drop`).
2. `Grill::stop`, accepted within `[runtime] stop_confirmation_timeout_secs`.
3. The grace: `stop_grace`, or the caller's default (`drain_timeout` in a deploy, 10 s otherwise).
4. `Grill::kill` and a confirmed exit.

Operator stops and retirements (`app_exit_wait`) read each instance's policy from the supervisor; the deploy worker gets it from the loop with `BeginRetire`, which already fences the instance from restarts.

**Validation.** The signal and durations parse into `StopSignal` and `StopDuration`, so an unknown signal, a number, `SIGKILL`, a zero or an over-an-hour duration is a TOML parse error with the field's line. `validate_app` refuses a `pre_stop_timeout` without a `pre_stop`, an empty program, and, on an `exec` or `script` app, a program that isn't an absolute path. Such an app's pre-stop runs on the host, so it joins the app's host binaries for placement (`WorkloadRequirements::host_binaries`) and for the node's `allowed_binaries` check.

**Deadlines.** The placement reconciler's deploy deadline adds `pre_stop_timeout` and `stop_grace` per replica. Its retirement deadline is still sized for the default grace; a longer stop overruns it, is retried on the next pass and joins the pending stop.

**Kubernetes.** `relish import` maps `terminationGracePeriodSeconds`, `lifecycle.stopSignal` and an exec `lifecycle.preStop` to the three fields and warns about the difference in how the bound works; `relish export` maps them back. `pre_stop_timeout` has no Kubernetes field.

### 5.9 Policies on the Apply Path (#1117)

`[policy.*]` is a declarative kind committed to Raft like a permission
(`RaftRequest::PolicySpec`, `PolicyDelete`; `DesiredState::policies`). A
policy holds a CEL `match` (optional) and `deny`, a `message` and an `action`
(`deny` or `warn`). The leader evaluates every policy against every applied
app with one pure function, `crate::policy::evaluate`; the state machine never
evaluates one.

- **Where.** `cluster_apply` calls `policy::admit` before anything is bound or
  written, under the committed policies with the config's own replacing any
  of the same name. A denial answers 422 with a `PolicyRefusal` body, writes
  nothing and records a `policy.denied` audit event. A job manifest's apps are
  judged the same way in `job_apply`, but without a fence (the state machine
  commits them later). The Lettuce runner judges each added or changed app in
  `apply_changes`; an adopted app is never judged.
- **Fence.** `DesiredState::policy_generation` moves by one on every applied
  policy write and delete. Each app write is wrapped in
  `RaftRequest::PolicyFenced { generation, change }`. The state machine applies
  `change` only while its generation still matches, and otherwise answers
  `CouncilResponse::PolicyGenerationMoved`. Both callers then judge the apps
  still to write again, up to three times. A config that writes policies and
  apps fences its apps on the generation plus the number of policies it writes,
  because its policy writes come first.
- **Input.** `app` (the declared spec as JSON, before images are bound, plus
  `name` and the resolved `namespace`), `caller` (`name`, `role`) and `source`
  (`apply` or `gitops`). Nothing else.
- **Commit checks.** `policy::check_policy`, run by `Config::validate`: a DNS
  label name, expressions of at most 1024 bytes, a message of at most 512,
  valid CEL, only the three variables, and only known `app` and `caller`
  fields. The `cel` crate has no cost limit, so the size cap bounds evaluation.
- **Failure.** An expression that errors or returns a non-boolean fails
  closed: a deny policy refuses, a warn policy warns. A committed policy that
  no longer compiles refuses every app apply until an admin replaces or
  deletes it. `POST /v1/policies/{name}/delete` is never judged.
- **Defaults (#1118).** A policy may carry a `default` table (stored as a JSON
  map in `PolicySpec::default`), and `deny` is optional once it does.
  `policy::judge_app` judges one app in a fixed order: every matching default
  in policy-name order, each `match` seeing the spec the policies before it
  left; then `validate_app_spec` on the filled spec; then every `deny` on it.
  A default fills a key only where the serialised spec holds nothing or
  `null`, recursing into tables both hold, so a value the app sets always
  wins. `admit` returns `Admitted::defaulted` (declared and effective spec per
  changed app) and `filled`; the app writes carry the effective spec. `fence`
  compares each app write with the declared app and, when they differ (a
  default, or an image bound to a digest), carries the declared spec:
  `PolicyFenced { generation, change, declared }`. The state machine stores it in `DesiredState::declared_apps`
  when the inner write lands, clears it on a fenced write without one, on any
  app write that changes the spec, and on delete. A write of the same spec (a
  GitOps adoption) keeps it. `GET /v1/apps` fingerprints the declared spec, so
  `relish apply --dry-run` shows a defaulted app as unchanged and lists each
  filled field under `Policies:`. When the policies move mid-apply and would
  fill a remaining app differently, the apply stops and asks to be run again.
  Commit checks refuse an empty or oversized (4096-byte) default, a field
  that isn't an app field or can't be filled (`name`, `namespace`,
  `replicas`, `ordinals`, `sidecar`, and the fields the apply path gates on
  the caller's token: `exec`, `script`, `volumes`, `config_file`, `init`,
  `runtime`), and a value serde won't read back into `AppSpec`. The token
  gates run on the declared spec, before policy, so a default must not be
  able to add what they'd have refused.
- **Jobs and audit (#1119).** `PolicySpec::applies_to` is `apps` (the default,
  so every earlier policy keeps its meaning), `jobs` or `both`. An expression
  sees `app` or `job` by the workload's kind, plus `workload`, bound to the
  same value; a policy may only name the variables its target gives it, and
  `workload` only has the fields in `WORKLOAD_FIELDS`, the intersection of
  `APP_FIELDS` and `JOB_FIELDS` (a test keeps both lists honest against
  serde). `judge_job` evaluates denies only: a `default` belongs to an apps
  policy. `evaluate` judges a config's apps then its jobs, `admit` judges
  both, and `job_apply` calls it whenever a manifest has apps or jobs; job
  writes stay unfenced, like the apps of a job manifest. Every other job
  route (`/v1/jobs/runs`, `/v1/batch`, `/v1/batch/array`,
  `/v1/batch/manifest`) judges each job in `job_api::admit_definition`
  (`PolicyCheck::Judge`; `job_apply` passes `Judged`) under the committed
  policies, as `JobDefinition::to_spec` (schedule and run policy put back),
  before its image is bound (#1424). A denial is the same 422 and
  `policy.denied` event; warnings come back as `policy_warnings`. `Finding` carries a
  `WorkloadKind` and names the workload (`policy p denies job ns/name: …`).
  `PolicyAction::Audit` puts a finding in `Verdict::audits`: the apply goes
  ahead, the caller isn't told, and `cluster_apply` and `job_apply` record one
  `policy.audited` event; the GitOps runner logs it to stderr. `GET
  /v1/policies/audit` (any token) runs `policy::audit::audit` on a copy of
  desired state in `spawn_blocking`: every committed app (declared spec where
  kept, else the stored one with its image read back as its tag) through
  `judge_app`, as the owner's token (with its role) for apps a controller
  owns (#1125), as `gitops` for apps GitOps owns and as the system caller
  `audit` otherwise, including an owned app whose token is gone, and every job definition (schedule restored, image read
  back) through `judge_job`. It changes nothing. `relish policy audit` prints
  it, and `relish wtf` reads it into a `policies` evidence row
  (`policy-would-deny`, `policy-audit`, `policy-broken`).
- **Cost and coverage (#1126, #1311).** `benches/policy.rs` (in `make bench`)
  measures `PolicySet::compile` and `judge_app` for 10, 100 and 1,000
  policies: about 115 µs per policy to compile, about 2 µs per policy per app
  to judge ([record](../qualification/2026-10-24-policy-evaluation-bench.md)).
  `relish apply` compiles once per apply; caching the set per generation
  would remove that if a cluster ever runs hundreds of policies.
  `examples/policies/platform.toml` holds the four common policies, checked
  offline by `tests/suite/platform_policies.rs` and on three nodes by
  `make test-cluster` (apply, GitOps, a leader kill, break-glass). The V02
  soak workload carries a deny, a warn and a default scoped to `soak-*`, and
  `sustained_check.py` fails a snapshot where one is missing or changed or
  the generation went backwards.

---

### 5.10 Runtime Knobs

An app can ask for added capabilities, namespaced sysctls and host devices (#1122):

```toml
[app.vpn]
image = "ghcr.io/example/vpn:2.0"

[app.vpn.runtime]
capabilities = ["NET_ADMIN"]
sysctls = { "net.ipv4.ip_forward" = "1" }
devices = [{ path = "/dev/net/tun", permissions = "rw" }]
```

**Two gates.** Each use needs the node and the token, the double gate host paths use. The token gate runs at apply, in the same place as `host-exec` for host binaries and host-path sources: a token with a `[permission]` block needs `host-exec` for an app with any knob, and its refusal names the first knob and "the token gate". The node gate is the `[runtime]` allowlist in `node.toml` (`allowed_capabilities`, `allowed_sysctls`, `allowed_devices`), checked by `WorkloadSupervisor::admit_runtime_knobs` in `admit_app` and again before every rolling or blue-green replacement, with a refusal that names the knob, the allowlist key and "the node gate".

**What can be asked for at all.** `grill::knobs` parses each field into a type that can't hold a value no node could allow. `Capability` holds a name from the 41 Linux capabilities, accepts `NET_ADMIN` or `CAP_NET_ADMIN`, and refuses `ALL` or any wildcard. `SysctlName` accepts only the per-namespace families (`net.*`, `kernel.shm*`, `kernel.msg*`, `kernel.sem`, `fs.mqueue.*`), and the node's `SysctlPattern` accepts those names or a `net.`/`fs.mqueue.` prefix ending in `.*`, so `kernel.panic` is refused in a node's list as well as an app's. `DevicePath` is an absolute path under `/dev` with no `.` or `..` part; `DevicePermissions` is a subset of `rwm`. `validate_app` refuses knobs on an `exec` or `script` app, which has no container to change.

**Escape-hatch capabilities.** Ten capabilities give near-root power inside the container's namespaces (`ESCAPE_CAPABILITIES`: `CAP_SYS_ADMIN`, `CAP_SYS_MODULE`, `CAP_SYS_RAWIO`, `CAP_SYS_PTRACE`, `CAP_SYS_BOOT`, `CAP_DAC_READ_SEARCH`, `CAP_MAC_ADMIN`, `CAP_MAC_OVERRIDE`, `CAP_BPF`, `CAP_PERFMON`). They aren't refused outright, because FUSE and some network tools genuinely need `CAP_SYS_ADMIN`. They're admitted only when a node names each one: there is no wildcard or pattern that could include them by accident, and Bun prints a startup warning for each one its `[runtime]` lists.

**Placement.** A node reports its allowlist in its capability report (`NodeCapabilities::runtime_knobs`), and only when its runtime applies knobs (`Grill::applies_runtime_knobs`, true for rootful runc and the mixed runtime's runc side). `WorkloadRequirements::runtime_knobs` carries the app's knobs and `node_can_run` admits a node only when its allowlist admits every one, so an app needing a knob that some nodes allow lands only on those. Otherwise it's unplaced with `NoCapableNode`.

**Applying them.** `generate_oci_spec` copies the knobs into the instance's `OciSpec` (`runtime_knobs`, not part of the OCI spec proper). Rootful runc's prepare step calls `grill::knobs::apply_to_spec` after the user namespace has set its default capabilities: added capabilities join the bounding, effective and permitted sets; sysctls go to `linux.sysctl`; each device is read from the host (`stat`) into `linux.devices` with an allow rule in `linux.resources.devices`. The network namespace belongs to the node's user namespace, not the container's, so container root can't write `net.*` sysctls; `take_network_sysctls` moves them out of the spec and Bun writes them with `ip netns exec`, as it writes the low-ports sysctls. runc writes the IPC ones in the container's own IPC namespace. ProcessGrill, Apple Container and rootless runc refuse a spec that carries knobs (`LaunchProblem::RuntimeKnobsUnsupported`) rather than run it without them. A sidecar's spec (`generate_sidecar_oci_spec`) carries no knobs: the gates admitted them for the app's container. It shares the app's network namespace, so the app's `net.*` sysctls reach it anyway.

**Kubernetes.** `relish import` maps `securityContext.capabilities.add`, the pod's `securityContext.sysctls` and device `hostPath` volumes; unknown capabilities, host-wide sysctls, `capabilities.drop` and `privileged` are warnings. `relish export` writes them back, devices as `hostPath` mounts reported as unsupported.

### 5.11 Resource Owners (#1125)

A controller is an ordinary app holding an API token. It claims an app by
applying it with `owner = "<its token's name>"` (`AppSpec::owner`), and from
then on `relish apply` and GitOps leave the app alone, the way GitOps leaves
alone what it didn't write. The rules live in `council::owner` as pure
functions, one per kind of change:

| Who | Does what | Result | Where |
|-----|-----------|--------|-------|
| anyone | apply declaring `owner = X` | allowed only for token `X` or an admin, else 403 | `judge_apply` |
| anyone | apply of an app owned by `X`, declaring `X` | written | `judge_apply` |
| anyone | apply of an app owned by `X`, declaring no owner | skipped, with a progress note; the rest applies | `judge_apply` |
| anyone | apply of an app owned by `X`, declaring `Y` | 409 until it's released | `judge_apply` |
| token `X` or an admin | stop, delete, rollback, release | allowed | `judge_command`, `judge_release` |
| anyone else | stop, delete, rollback, release | 409; `?force=true` doesn't help | `judge_command` |
| GitOps | an owned app in the diff | left out: no update, adoption or prune | `lettuce::diff` |
| GitOps | git declaring an `owner` | refused, in the partial result | state machine |

- **Where.** `cluster_apply` calls `skip_owned_apps` on the leader before
  policy admission, so a skipped app is neither judged nor written.
  `job_apply` does the same for the apps of a job manifest (#1429): the
  state machine's backstop only checks that a write keeps the stored owner,
  not that the caller speaks for it, so the API is the only place that can.
  `cluster_app_change` (stop, delete, release) and `rollback_handler` check
  before forwarding to the leader, because a forwarded request reaches the
  leader as the service principal. The bootstrap window and the service
  principal count as admins.
- **Defaults.** The checks run on the declared spec, before policy defaults
  (#1118) fill it, so `owner` is in `policy::check::UNFILLABLE_FIELDS`: a
  policy whose `default` sets it is refused at commit, or a default could
  claim apps for a controller their caller can't speak for.
- **Backstop.** Two writers can read the same owner and race. The state machine
  refuses an `AppSpec` (or a leased one) whose `owner` differs from the stored
  app's, with `stored_owner_conflict`, which compares two strings and needs no
  caller. Only `RaftRequest::AppRelease` clears an owner; it's refused for an
  app that isn't there or has none.
- **Not a version.** `rollout_fingerprint` and the node's placement fingerprint
  set `owner` aside, and a claim doesn't count as a changed spec for a failed
  rollout. Claiming or releasing restarts nothing. A rollback keeps today's
  owner, whatever the old spec said.
- **Reads.** `GET /v1/diagnostics/apps` carries `owner`, and a policy sees it
  as `app.owner`. The JSON a controller reads and writes is documented in the
  manual's "Writing a controller" chapter and pinned by
  `src/bun/api/contract_tests.rs`.
- **Scope.** A controller's token is a `deployer` scoped to its namespace; the
  existing scope checks keep it there. Only apps carry an owner: a scoped token
  can't write namespaces, permissions or policies anyway.
- **The example.** `examples/controller/main.rs` keeps one of a cold-standby
  pair running, from two `[app.*]` tables or from a template with two
  `[use.*]` tables (`postgres-pair.toml`), in which case it applies the
  active `use` with its own `owner` and `namespace` as parameters and the
  apply endpoint expands it. The 0.2.4 exit test runs it on three nodes
  (`tests/cluster_failover.rs`), failing over when the node under the active
  member is killed, with Postgres replaced by a `sleep` stand-in because the
  fixture runs host processes.

---

### 5.12 Templates (#1124)

`[template.NAME]` holds typed `params` (`string`, `int` or `bool`, optionally
with a `default`; without one, required) and a `body` string of ordinary
`[app.*]` and `[job.*]` TOML with `${param}` placeholders. `[use.NAME]` holds
`template` and one value per parameter. `config::template` owns both.

- **Where it expands.** `template::expand` runs in three places, always
  before validation and policy: `relish::compile::compile_sources` (the
  compiler `relish compile` and Lettuce share), `relish::manifest::parse`
  (`apply`, `apply --dry-run`, `lint`, `diff`, single-file `compile`,
  `policy test`), and the apply endpoint for a manifest that arrives
  unexpanded. `relish apply` sends the expanded config, so the endpoint's
  call is a no-op for it. `Config::validate` refuses a `use` that's still
  there, so nothing can skip expansion silently.
- **In a tree.** `compile_sources` collects every template from every file
  first (a name declared twice is refused), then expands each file's uses
  before that file's `_defaults.toml` and directory namespace apply, so an
  expanded app is an ordinary app of the file holding its `use`. A later
  file overriding an app is a warning for `relish compile`, but refused
  when a template made either one, under both duplicate policies.
- **Substitution.** `scan` walks the body tracking whether it's outside
  strings, in a basic string (one line or many), a literal string (one line
  or many) or a comment. In a basic string a value is escaped; outside
  strings a string value must match `[A-Za-z0-9_-]+`; in a literal string
  it can't hold `'` or control characters. Integers and booleans go in as
  written everywhere. Comments are copied, placeholders and all. `$${` is a
  literal `${`. A value is never scanned again. So no value can add a key
  or table to the body, and expansion is deterministic.
- **Refusals** (`TemplateError`): an unknown template, unknown parameter,
  missing required parameter or wrong type; a placeholder no parameter
  resolves, checked on every template whether or not it's used; a value that
  can't go where its placeholder is; a body that isn't valid TOML, or holds
  anything but apps and jobs; a body that declares or uses a template (no
  nesting); an expanded name a declared resource or another use already has;
  template and use names that aren't DNS labels, and a parameter named
  `template`; and a use whose expansion would pass `MAX_EXPANDED_BYTES`
  (4 MiB), or all the uses of one expansion (a manifest, or one file of a
  tree) `MAX_TOTAL_EXPANDED_BYTES` (16 MiB), checked as the text grows so
  nothing past the cap is allocated (#1425).
- **What's stored.** Only ordinary apps and jobs. Each expanded one carries
  `provenance: { template, use }` (`AppSpec::provenance`,
  `JobSpec::provenance`), the one wire and state change. It's part of the
  spec, so the GitOps declared-spec diff compares it like any field: git's
  expansion and the stored spec agree, and a re-sync writes nothing. Like
  `owner` (5.11), it isn't part of the version: `rollout_fingerprint` and the
  node's placement fingerprint set it aside, so moving an app into or out of
  a template restarts nothing when the expansion is the same. Policies see
  it as `app.provenance` or `job.provenance`; a policy `default` can't set
  it. It is not a trust signal (#1430): any manifest may declare a template
  of any name, and a hand-written spec may set the field, which the apply
  endpoint can't tell from `relish`'s local expansion. A policy may aim a
  rule at it, never exempt on it. A body may declare an `owner`, and the owner rules judge the expanded
  app exactly as they judge a hand-written one. `relish apply
  --dry-run` shows it as `from use.NAME (template NAME)`.
- **Not done.** No registry: templates come from files or git. No logic in
  templates, and no nesting.

## 6. Configuration

All deploy configuration lives under the `[app.<name>.deploy]` TOML section. Defaults are set per-directory in a `_defaults.toml` file, merged into each app by `relish compile`. There is no cluster-level `[defaults.app.deploy]` section — the block shown below is planned, not implemented.

```toml
# Per-app deploy configuration
[app.web.deploy]
strategy = "rolling"        # "rolling" (default) or "blue-green"
max_surge = 1               # max extra instances during rolling update (default: 1)
drain_timeout = "30s"       # time to wait for in-flight connections to drain (default: 30s)
health_timeout = "60s"      # time to wait for new instance health check (default: 60s)
auto_rollback = false       # halt on failure instead of the default active revert

# Per-app autoscale configuration (optional)
[app.web.autoscale]
metric = "cpu"              # "cpu" or "memory"
target = "70%"              # target utilisation of the request
min = 2                     # minimum replicas
max = 10                    # maximum replicas
```

**Cluster-wide defaults (planned — not yet implemented):**

The intent is a defaults section in the cluster config so every app inherits the
same deploy policy. Today the only defaulting mechanism is the per-directory
`_defaults.toml` merged by `relish compile`; the `[defaults.app.deploy]` section
below is not read by anything yet.

```toml
# Planned. Not currently parsed.
[defaults.app.deploy]
strategy = "rolling"
auto_rollback = true        # default to active revert for all apps
drain_timeout = "30s"
health_timeout = "60s"
```

**Full configuration reference:**

| Field | Type | Default | Description |
|---|---|---|---|
| `strategy` | `"rolling"` or `"blue-green"` | `"rolling"` | Deploy strategy |
| `max_surge` | integer | `1` | Maximum extra instances during rolling update. Higher values trade resources for speed. An app with a managed volume always rolls with `max_surge = 0` (and `max_unavailable` at least 1, blue-green included), since old and new instances would share the volume. |
| `drain_timeout` | duration string | `"30s"` | How long to wait for in-flight connections to complete before force-stopping |
| `health_timeout` | duration string | `"60s"` | How long to wait for a new instance's health check endpoint to return success |
| `auto_rollback` | boolean | `true` | `true`: actively revert all upgraded instances on failure. `false`: halt and leave the mixed old/new state for the operator to decide. |
| `autoscale.metric` | `"cpu"` or `"memory"` | (none) | Metric to scale on. Autoscaling is disabled if this section is absent. Any other value fails validation; `"memory"` also requires a `memory` request. |
| `autoscale.target` | string (`"70%"` or `"0.7"`) | (required if autoscale set) | Target utilisation of the per-replica request (of one core for CPU with no request) |
| `autoscale.min` | integer | (required if autoscale set) | Minimum replica count |
| `autoscale.max` | integer | (required if autoscale set) | Maximum replica count |

---

## 7. Failure Modes

### 7.1 Health Check Failure During Rollout

**Trigger:** A new instance doesn't pass its health check within `health_timeout`.

**Behaviour:** The failed instance is stopped immediately (it never entered the routing pool, so no drain is needed). The deploy transitions based on `auto_rollback`:

- `true` (default): Deploy enters `Reverting` state. The system starts rolling back already-upgraded instances to the previous version using the same rolling mechanism. After revert completes, the deploy enters `RolledBack` state. If the previous version can't be restored, the healthy upgraded instances keep serving and the deploy enters `Halted`.
- `false`: Deploy enters `Halted` state. Already-upgraded instances continue serving on the new version. Remaining instances stay on the old version. `relish status` shows the partial state. The operator must intervene.

In a cluster either outcome marks the app `RolloutFailed` (Section 5.2), so the leader stops releasing the spec to further nodes.

**Detection:** Bun on the target node performs the health check (HTTP GET to the configured health endpoint). If the endpoint returns a non-2xx status or the connection times out for the entire `health_timeout` duration, the instance is considered unhealthy. Bun reports this to the leader via the reporting tree.

### 7.2 Node Failure Mid-Deploy

**Trigger:** A node becomes unreachable (detected via gossip protocol heartbeat timeout) while a rollout step is executing on that node.

**Behaviour depends on which phase the step was in:**

- **Starting / HealthChecking:** The new instance on the failed node is considered lost. The leader reschedules the step to a different healthy node. If no healthy node has capacity, the deploy halts.
- **RoutingUpdate / Draining:** The old instance was already removed from the routing pool. The new instance may or may not be healthy. The leader checks whether the new instance was added to the routing pool before the node failed. If yes, the routing entry is stale (points to a dead node) and must be removed. The leader reschedules the app instance to a surviving node.
- **Any phase:** If the failed node was running already-completed steps (instances from earlier steps that are now on the new version), those instances are lost and must be rescheduled to surviving nodes. This is handled by the normal Meat rescheduling logic, independent of the deploy system.

**Mitigation (original design):** deploy state persisted in Raft after every phase transition, so a new leader could resume from the last committed phase.

**Shipped (0.2.1):** there is no leader-held deploy state to persist. A node's own rollout carries on whatever happens to the leader, and a new leader reads each node's converged version and health from its reports and releases the next node from there (`new_leader_resumes_node_by_node_release_from_reports`).

### 7.3 Drain Timeout Exceeded

**Trigger:** In-flight connections to a draining instance don't complete within `drain_timeout`.

**Behaviour:** Bun sends SIGTERM to the container. After a 10-second grace period, SIGKILL is sent if the process is still running. The step proceeds to completion. The drain outcome is recorded as `ForcedTimeout` with the number of remaining connections in the deploy history.

**Impact:** Clients with in-flight requests to the killed instance experience connection resets. This is logged as a warning in Ketchup. If forced drains happen frequently, the operator should increase `drain_timeout`.

### 7.4 Dependency Job Failure

**Trigger:** A `run_before` job fails (non-zero exit code) after exhausting its configured retry count.

**Behaviour:** The deploy transitions to `Failed` with a clear error message: "dependency job 'db-migrate' failed after 3 attempts (exit code 1)". The app isn't modified. No rolling update begins. The operator must fix the job and re-deploy.

**Mitigation:** The job's stderr output is captured in Ketchup logs and linked from the deploy history entry. `relish logs job.db-migrate` shows the failure output.

### 7.5 Autoscaler Fighting with Manual Scale

**Trigger:** An operator manually sets `replicas = 5` while the autoscaler's current evaluation says the app needs 3 replicas.

**Behaviour:** Manual scale operations (via `relish scale` or a git push changing the `replicas` field) reset the autoscaler's runtime override. The autoscaler respects the new base count and re-evaluates from there. However, if the autoscaler's evaluation window still shows low utilisation, it may scale back down to 3 on the next evaluation cycle.

**Mitigation:** To temporarily override the autoscaler, operators can:

- Set `min` to the desired floor: `relish config set app.web.autoscale.min 5`
- Disable autoscaling temporarily: `relish config set app.web.autoscale.enabled false`
- Both are runtime overrides stored in Raft and don't modify the git config.

### 7.6 Image Pull Failure

**Trigger:** The new image cannot be pulled to the target node (Pickle unavailable, external registry unreachable, image tag doesn't exist).

**Behaviour:** The step fails during the `Starting` phase. The deploy follows the same failure path as a health check failure (halt or revert based on `auto_rollback`). The error message includes the specific pull failure reason.

### 7.7 Insufficient Cluster Resources

**Trigger:** There isn't enough CPU/memory on any eligible node to start a new instance alongside the existing one (required for `max_surge`).

**Behaviour:** The deploy remains in `Rolling` state with the current step stuck in `Pending`. Meat's scheduler emits a warning: "deploy blocked: no eligible node has capacity for app.web (needs 500m CPU, 512Mi memory)". If resources aren't freed within a configurable deploy timeout (default: 30 minutes), the deploy halts.

---

## 8. Security Considerations

### 8.1 Deploy Permission Model

Deploy operations require the `deploy` action in the user's Permission grant:

```toml
[permission.deployer]
actions = ["deploy", "scale", "logs", "metrics"]
apps = ["web", "api"]
```

A user with this permission can deploy and scale the `web` and `api` apps but can't modify other apps, manage cluster configuration, or access secrets. The API layer enforces the permission (Raft write operations) before the deploy enters the state machine.

**Permission checks for deploy operations:**

| Operation | Required Action | Scope |
|---|---|---|
| `relish deploy` | `deploy` | Must include the target app |
| `relish rollback` | `deploy` | Must include the target app (rollback is a deploy) |
| `relish scale` | `scale` | Must include the target app |
| `relish deploy --continue` (resume halted deploy) | `deploy` | Must include the target app |
| `relish deploy --cancel` | `deploy` | Must include the target app |
| Autoscaler scale events | (system-initiated) | No user permission required; logged as actor `Autoscaler` |
| GitOps-triggered deploys | (system-initiated) | Lettuce acts with system privileges; logged with commit SHA |

### 8.2 Rollback Authorisation

Rollback is authorised with the same `deploy` permission as forward deploys. There's no separate rollback permission because a rollback is operationally identical to a deploy (it uses the same rolling mechanism with the previous image). Restricting rollback more than deploy would slow incident response.

Automatic rollback (triggered by `auto_rollback = true`) is a system-initiated action. It doesn't require user authorisation because the user already authorised the original deploy with the rollback policy. The auto-rollback is logged with actor `Rollback { from_deploy_id }`.

### 8.3 Audit Trail Integrity

Every deploy event is recorded in three places:

1. **Raft log:** Rollout outcomes (`RolloutReleased`, `RolloutFailed`) are Raft entries, replicated across council members. Individual phase transitions aren't.
2. **Ketchup structured logs:** Each phase transition emits a structured log event with deploy ID, app name, phase, actor, and timestamp. These logs can be forwarded to external systems (Datadog, Splunk, etc.) for compliance.
3. **Deploy history:** each node keeps the newest fifty entries per app, merged across the cluster by `relish history`.

The Raft log is append-only by design, and Raft requires a quorum of council members to accept writes, so one compromised node can't rewrite the rollout records. The per-node deploy history has no such protection.

---

## 9. Performance

### 9.1 Deploy Speed

**Time to first healthy instance:** The critical path for a single rollout step is:

```
Image pull (if not cached)  +  Container start  +  Health check pass  =  Time to healthy
        ~0s (Pickle local)       ~500ms                1-30s (app-dependent)
```

For images hosted on Pickle (the common case), the image is already replicated to multiple nodes. The pull is a local file read, not a network transfer. Container start time is dominated by the application's startup time, not the container runtime overhead. The health check interval determines how quickly a healthy instance is detected (default: 5-second intervals).

**Time for N replicas (rolling):** With `max_surge = 1`, total deploy time is approximately `N * (time_to_healthy + drain_timeout)`. For a typical web application with 3 replicas, 5-second startup, and 30-second drain: `3 * (5s + 30s) = ~105s`. With `max_surge = 2`, two steps run in parallel, reducing this to approximately `ceil(3/2) * 35s = ~70s`.

**Time for N replicas (blue-green):** All new instances start simultaneously. Total deploy time is approximately `max(time_to_healthy across all instances) + drain_timeout`. For the same 3-replica app: `5s + 30s = ~35s`. Faster, but requires double the resources during transition.

### 9.2 Drain Overhead

Connection draining adds `drain_timeout` (default 30s) to each rolling step. In practice, most HTTP connections complete in milliseconds, so the actual drain time is much shorter than the timeout. The drain waits only until in-flight connections reach zero, not for the full timeout.

For applications with long-lived connections (WebSockets, gRPC streams), the drain timeout should be set higher. The deploy history records actual drain duration vs. configured timeout, so operators can tune the value based on observed behaviour.

### 9.3 Rollback Speed

Rollback speed depends on the mode:

- **Halt mode:** Instant. The deploy simply stops. No instances are modified.
- **Active revert mode:** Approximately the same time as a forward deploy of the affected instances. If 2 of 3 instances were upgraded before the failure, the revert rolls 2 instances back, taking `2 * (time_to_healthy + drain_timeout)`.

For blue-green deploys, rollback is near-instant: switch traffic back to the blue set (a routing pool swap, no container starts needed) and then stop the green set.

### 9.4 Autoscaler Responsiveness

The autoscaler evaluates metrics every 30 seconds. With a 5-minute evaluation window (default), it takes at least 5 minutes of sustained high utilisation before a scale-up is triggered. Scale-up then takes `time_to_healthy` per new instance (typically 5-30 seconds). Total time from load increase to additional capacity: approximately 5-6 minutes.

The cooldown period (default: 3 minutes) prevents rapid oscillation but means the system can't react to sub-3-minute traffic spikes with additional scale-downs after a scale-up.

---

## 10. Testing Strategy

### 10.1 Zero-Downtime Deploy Verification

The integration test "rolling deploy with zero downtime (continuous health probe)" works as follows:

1. Deploy a test app with 3 replicas.
2. Start a continuous health probe that sends HTTP requests to the app every 100ms via Wrapper.
3. Trigger a rolling deploy to a new version of the test app.
4. Verify that every health probe request succeeds throughout the deploy (zero 5xx responses, zero connection refused errors).
5. Verify that the deploy completes and all instances are on the new version.
6. Report the total deploy time and the number of successful probe requests.

This test exercises the full deploy path: Meat orchestration, Bun container lifecycle, Wrapper routing pool updates, Onion service map updates, and connection draining.

### 10.2 Rollback Testing

Two integration tests cover rollback:

**"deploy broken image, verify auto-rollback to previous version":**

1. Deploy a test app with a healthy image.
2. Deploy a new version that intentionally fails health checks (the built-in test server supports configurable health check behaviour).
3. Verify that the deploy detects the failure, reverts all instances, and the app is fully on the previous version.
4. Verify that the deploy history records the rollback with the correct reason.

The shipped tests: `auto_rollback_after_a_retirement_restores_old_capacity`, `auto_rollback_of_a_stop_first_volume_app_restarts_the_old_instance` and `abort_keeps_healthy_new_instances_when_old_capacity_cannot_return` (agent unit tests), the node-by-node planner's unit tests in `src/cluster/rollout.rs`, and, under `make test-cluster`, `three_node_one_per_node_app_keeps_two_replicas_serving_during_redeploy`, `bad_version_in_cluster_becomes_rollout_failed_and_stops_retrying` and `rollback_works_after_every_node_restarts` in `tests/placement.rs`. There is no "kill the leader mid-deploy" chaos test: with no leader-held deploy state, the planner's resume-from-reports unit test covers that path.

### 10.3 Dependency Ordering Verification

**"run_before ordering respected (job completes before app starts)":**

1. Define a job with `run_before = ["app.test"]` that writes a marker file to a shared volume and then exits.
2. Deploy the test app, which checks for the marker file on startup and fails health checks if the file is missing.
3. Verify that the app starts successfully (the job ran first and created the marker file).
4. Verify deploy history shows the dependency job completion before the rolling phase began.

### 10.4 Blue-Green Deploy Verification

**"blue-green deploy, verify traffic cutover":**

1. Deploy a test app with `strategy = "blue-green"` and 2 replicas.
2. Start a continuous health probe.
3. Deploy a new version that returns a version header in its HTTP response.
4. Verify that the probe responses switch from v1 to v2 atomically (no mixed-version responses during the cutover).
5. Verify zero dropped requests during the transition.

### 10.5 Autoscale Testing

**"autoscale up on CPU pressure, scale down on relief":**

1. Deploy a test app with `autoscale.metric = "cpu"`, `target = "50%"`, `min = 1`, `max = 5`.
2. Generate CPU load on the test app instances.
3. Verify that the autoscaler increases replicas within the evaluation window + cooldown period.
4. Remove CPU load.
5. Verify that the autoscaler decreases replicas after the evaluation window shows low utilisation.
6. Verify that replica count stays within `[min, max]` bounds throughout.

---

## 11. Prior Art

### 11.1 Kubernetes Deployment Controller

Kubernetes uses a Deployment resource that manages ReplicaSets, which in turn manage Pods. A rolling update creates a new ReplicaSet, scales it up while scaling the old ReplicaSet down, governed by `maxSurge` and `maxUnavailable` parameters. Rollback is handled by reverting to a previous ReplicaSet revision stored in the Deployment's history.

- **Kubernetes Deployment design:** https://github.com/kubernetes/design-proposals-archive/blob/main/apps/deployment.md
- **Rolling update strategy:** https://kubernetes.io/docs/concepts/workloads/controllers/deployment/#rolling-update-deployment

**What we borrow:** The rolling update model with `max_surge` controlling the pace of replacement. The concept of deploy history with numbered revisions. Health-check-gated progression (a new Pod must be Ready before the old one is removed).

**What we do differently:**

- **Single resource.** Kubernetes requires a Deployment (desired state), ReplicaSet (version tracking), Service (routing), and optionally an Ingress (external access) and HPA (autoscaling). Reliaburger's App resource is all of these in one declaration.
- **Built-in connection draining.** Kubernetes relies on the Pod's `terminationGracePeriodSeconds` and the application handling SIGTERM correctly. There's no coordination between the ingress controller and the Pod lifecycle; traffic can hit a Pod that is shutting down during the gap between endpoint removal and actual Pod termination. Reliaburger's deploy system explicitly coordinates Wrapper routing pool removal, drain completion, and container stop as sequential phases.
- **Auto-rollback as config, not a separate controller.** Kubernetes doesn't natively auto-rollback. Argo Rollouts or Flagger must be installed as separate controllers to add this capability. Reliaburger's `auto_rollback = true` is a single boolean in the app config.

### 11.2 Nomad Job Updates

Nomad's job update stanza controls rolling updates with `max_parallel`, `health_check`, `min_healthy_time`, `healthy_deadline`, and `auto_revert` parameters. Nomad's model is closer to Reliaburger's: a single job definition includes the update strategy, and the scheduler coordinates the rollout.

- **Nomad update stanza:** https://developer.hashicorp.com/nomad/docs/job-specification/update

**What we borrow:** The integrated update strategy within the resource definition (no separate controller). The `auto_revert` concept (Nomad's equivalent of `auto_rollback`).

**What we do differently:** Nomad doesn't have built-in ingress or service mesh, so connection draining during deploys requires Consul Connect or an external load balancer. Reliaburger's Wrapper is tightly integrated with the deploy system, making drain coordination a first-class operation rather than an external concern.

### 11.3 Spinnaker

Spinnaker is a standalone continuous delivery platform that supports multiple deployment strategies (rolling, blue-green, canary) with manual approval gates and automated analysis. It's designed for large organisations with complex multi-stage pipelines.

- **Spinnaker architecture:** https://spinnaker.io/docs/concepts/

**What we borrow:** The concept of deploy pipelines with multiple strategies available per application. The deploy history and audit trail as first-class features.

**What we do differently:** Spinnaker is a large, complex system that deploys to other platforms (Kubernetes, AWS, GCE). Reliaburger's deploy system is embedded in the orchestrator itself, with no external dependencies. There's no pipeline DSL; the deploy strategy is a few lines of TOML config.

### 11.4 Argo Rollouts

Argo Rollouts is a Kubernetes controller that provides advanced deployment strategies (canary, blue-green, progressive delivery) as a replacement for the built-in Deployment controller. It uses custom resources (Rollout) and integrates with service meshes and ingress controllers for traffic management.

- **Argo Rollouts architecture:** https://argoproj.github.io/argo-rollouts/architecture/

**What we borrow:** The concept of analysis-driven rollouts (health checks gating progression). The blue-green strategy with traffic switching.

**What we do differently:** Argo Rollouts exists because Kubernetes's built-in deploy capabilities are insufficient. It adds a CRD, a controller, and requires integration with a service mesh or ingress controller for traffic splitting. Reliaburger's deploy system is built in, with traffic management (Wrapper) and service discovery (Onion) already integrated. There's no need for a separate controller or CRD.

---

## 12. Libraries & Dependencies

| Crate | Purpose |
|---|---|
| `tokio` | Async runtime for the deploy orchestration loop, timeout handling (`tokio::time::timeout` for health check and drain deadlines), and concurrent step execution when `max_surge > 1`. |
| `serde` / `serde_json` | Serialisation of deploy state for Raft persistence and API responses. |
| `toml` | Parsing `[app.<name>.deploy]` and `[app.<name>.autoscale]` configuration sections. |

**State machine pattern:** The deploy state machine is implemented as an `enum`-based state machine (Rust's `enum` variants represent states, `match` expressions enforce valid transitions). No external state machine crate is needed; Rust's type system ensures exhaustive handling of all states. The pattern is:

```rust
impl DeployState {
    pub fn transition(&mut self, event: DeployEvent) -> Result<(), InvalidTransition> {
        self.phase = match (&self.phase, event) {
            (DeployPhase::Pending, DeployEvent::DepsDispatched) => DeployPhase::RunningPreDeps,
            (DeployPhase::RunningPreDeps, DeployEvent::AllDepsSucceeded) => DeployPhase::Rolling,
            (DeployPhase::RunningPreDeps, DeployEvent::DepFailed(e)) => DeployPhase::Failed { error: e },
            (DeployPhase::Rolling, DeployEvent::AllStepsCompleted) => DeployPhase::Completed,
            (DeployPhase::Rolling, DeployEvent::StepFailed(e)) if self.request.config.auto_rollback => {
                DeployPhase::Reverting
            }
            (DeployPhase::Rolling, DeployEvent::StepFailed(e)) => {
                DeployPhase::Halted { failed_step: self.current_step, error: e }
            }
            (DeployPhase::Reverting, DeployEvent::RevertCompleted) => DeployPhase::RolledBack,
            (phase, event) => return Err(InvalidTransition { from: phase.clone(), event }),
        };
        self.phase_transitions.push(PhaseTransition {
            to: self.phase.clone(),
            at: chrono::Utc::now(),
        });
        Ok(())
    }
}
```

This approach ensures that invalid state transitions (e.g., `Completed` -> `Rolling`) are caught at compile time via the exhaustive `match`, and any unhandled combination returns an `InvalidTransition` error rather than silently corrupting state.

---

## 13. Open Questions

### 13.1 Canary Deploys (Percentage-Based Traffic Splitting)

Should Reliaburger support canary deploys where a small percentage of traffic (e.g., 5%) is routed to the new version before proceeding with the full rollout?

**Implementation considerations:**

- Wrapper would need weighted routing (e.g., 95% to v1 backends, 5% to v2 backends). This is straightforward to implement in the reverse proxy but adds complexity to the routing pool model (currently backends are equally weighted).
- The canary phase would need success criteria: what metric or health signal promotes the canary to full rollout? Options include error rate, latency percentile, or manual approval.
- Canary adds a new state to the deploy state machine: `Canary` between `RunningPreDeps` and `Rolling`.

**Current stance:** Not in v1. Rolling deploys with `max_surge = 1` and `auto_rollback = true` provide adequate safety for the target audience. Canary deploys are most valuable for large-scale services (thousands of replicas) where rolling through all instances is slow and risky. The typical Reliaburger cluster (2-200 nodes) doesn't need this complexity.

### 13.2 A/B Testing Support

Should the deploy system support A/B testing (routing different user segments to different versions based on request attributes like headers, cookies, or IP ranges)?

**Implementation considerations:**

- Requires request-level routing decisions in Wrapper (inspect headers/cookies to choose backend version). This is architecturally different from instance-level load balancing.
- A/B testing is an application concern (product experimentation) rather than an infrastructure concern (safe deployments). Mixing the two in the deploy system adds conceptual complexity.

**Current stance:** Not in scope. A/B testing should be handled at the application layer (feature flags, experiment frameworks). The deploy system is concerned with safely rolling out a single version, not managing multiple concurrent versions for product experimentation.

### 13.3 Deploy Approval Gates

Should the deploy system support manual approval gates (e.g., "deploy to staging automatically, wait for human approval, then deploy to production")?

**Implementation considerations:**

- This is a pipeline concern rather than a single-cluster concern. Reliaburger manages one cluster; multi-stage pipelines (staging -> production) span multiple clusters.
- For single-cluster gates (e.g., "pause after canary phase for approval"), a `Paused` state could be added to the deploy state machine with a `relish deploy --approve` command to resume.
- Lettuce (GitOps) already provides a natural approval gate: the git merge/PR process. The deploy only happens when the change is merged.

**Current stance:** Not in v1. Git-based approval via Lettuce covers the majority of use cases. Explicit in-deploy approval gates may be added in v2 alongside canary support, where pausing after the canary phase for human review would be the primary use case.

### 13.4 Multi-App Atomic Deploys

Should the system support deploying multiple apps atomically (e.g., deploy both `api` and `web` together, rolling back both if either fails)?

**Implementation considerations:**

- The `run_before` dependency mechanism handles sequential ordering (migration before app). Atomic multi-app deploys are a different concern: parallel deploys with a shared fate.
- Requires a `DeployGroup` concept that wraps multiple `DeployState` instances and coordinates their success/failure.

**Current stance:** Not in v1. The `run_before` mechanism and the ability to deploy multiple apps from a single TOML file (each app rolls independently) cover the common case. True atomic multi-app deploys are a v2 candidate.
