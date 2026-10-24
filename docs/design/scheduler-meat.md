# Meat: The Reliaburger Scheduler

## 1. Overview

Meat is the centralized scheduling engine of the Reliaburger container orchestrator. It runs exclusively on the current Raft leader node as a dedicated async task and is responsible for all workload placement decisions in the cluster: placing App replicas across nodes, granting job tasks to workers in chunks, releasing rollouts node by node, enforcing namespace resource quotas, and driving autoscaling reactions.

### Role in the System

Meat is the single decision-maker for "what runs where." Every App placement, Job allocation, rolling deploy step, and autoscale event flows through Meat. It consumes desired state (from the Raft log or Lettuce GitOps engine), combines it with the cluster's reported runtime state (from the hierarchical reporting tree), and emits scheduling decisions that are committed to Raft and distributed to Bun agents on target nodes.

### Key Design Decisions

- **Single-leader scheduling.** All scheduling decisions are made on one node. This eliminates coordination overhead between multiple schedulers and makes the Raft log the single source of truth for placement decisions. The tradeoff -- a single point of decision-making -- is mitigated by fast leader failover (< 5 seconds) and the delegated batch model.

- **Delegated job execution.** Meat does not place individual job tasks. Every job (a TOML job, a `relish batch` array, a cron occurrence, a `run_before` hook) is a run of a durable definition, and the leader grants chunks of task indices to workers that report how many requests fit. Workers run the attempts and the leader accepts aggregate outcomes, so the Raft log grows with chunks, not tasks. §5.2 and the [delegated task-array appendix](#delegated-task-array-implementation-020) describe it; the job qualification records in `docs/qualification/` hold the measured throughput.

- **Bin-packing first.** Meat uses a bin-packing algorithm as its primary placement strategy, maximising node utilisation before spreading to new nodes. This reduces the number of active nodes under low load and improves cache locality for images already present on a node.

- **Dedicated CPU budget (planned — not yet implemented).** The intent is that when the cluster exceeds 1,000 nodes, Meat's async task is pinned to a specific CPU core on the leader node, ensuring that API serving, Brioche UI, metrics queries, and other leader responsibilities cannot starve the scheduling loop. No CPU-core pinning exists in the code today; the scheduling loop is an ordinary tokio task sharing the runtime.

- **Learning period before scheduling.** After a leader election, Meat enters a learning period (collecting StateReports from nodes) and does not make scheduling decisions until it has a sufficiently complete view of the cluster (95% of nodes reported or 15-second timeout). This prevents incorrect placements based on stale or incomplete state.

---

## 2. Dependencies

| Component | Dependency Type | Why |
|-----------|----------------|-----|
| **Raft (Council)** | Hard | Meat reads desired state from the Raft log and commits scheduling decisions to it. Meat only runs on the Raft leader. |
| **Mustard (Gossip)** | Hard | Provides cluster membership, node liveness, leader identity broadcast, and per-node resource summaries (CPU/memory/GPU capacity and utilisation). |
| **Hierarchical Reporting Tree** | Hard | Nodes report detailed runtime state (running apps, health, resource usage, job completions) to their assigned council member, which aggregates for the leader. This is Meat's primary source of "what is actually running." |
| **Bun (Agent)** | Hard | Executes scheduling decisions on each node. Bun starts/stops containers, enforces cgroups, and reports back via the reporting tree. |
| **Grill (Container Runtime)** | Indirect | Bun uses Grill to start containers. Meat does not interact with Grill directly but must account for Grill's startup latency in scheduling decisions. |
| **Lettuce (GitOps)** | Soft | When GitOps is enabled, Lettuce prepares change sets from git and forwards deploy requests to the leader. Meat processes these identically to CLI/API deploys. |
| **Mayo (Metrics)** | Soft | The leader's autoscaler reads per-instance `process_cpu_percent` and `process_memory_bytes` from its rollup store, which every node's rollup worker feeds once a minute. No custom metrics. |
| **Pickle (Registry)** | Soft | Meat checks image availability on target nodes when making placement decisions. Scheduling to a node that already has the image cached avoids pull latency. |
| **Wrapper (Ingress)** | Soft | During rolling deploys, Meat coordinates with Wrapper to add/remove instances from the routing pool and wait for connection draining. |
| **Sesame (Security)** | Soft | Meat validates that deployers have the required permissions (e.g., `host-exec` for process workloads) before accepting scheduling requests. |

---

## 3. Architecture

### Internal Structure

Meat is structured as a set of cooperating async tasks within the leader's Bun process. It is not a separate binary or process -- it is compiled into the single Reliaburger binary and activated only on the current leader node.

```
┌──────────────────────────────────────────────────────────────────┐
│                        PATTY SCHEDULER                           │
│                    (leader node only)                             │
│                                                                  │
│  ┌──────────────┐   ┌──────────────┐   ┌───────────────────┐    │
│  │  Scheduling   │   │   Deploy     │   │   Autoscale       │    │
│  │  Queue        │   │   Controller │   │   Controller      │    │
│  │              │   │              │   │                   │    │
│  │ - App place  │   │ - Rolling    │   │ - Metric poll     │    │
│  │ - Job alloc  │   │ - Rollback   │   │ - Scale decision  │    │
│  │ - Rebalance  │   │ - Dep order  │   │ - Cooldown        │    │
│  └──────┬───────┘   └──────┬───────┘   └────────┬──────────┘    │
│         │                  │                     │               │
│         ▼                  ▼                     ▼               │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │                   Placement Engine                          │ │
│  │                                                             │ │
│  │  ┌─────────┐  ┌──────────┐  ┌──────────┐  ┌────────────┐  │ │
│  │  │ Filter  │→ │  Score   │→ │  Select  │→ │  Commit    │  │ │
│  │  │ Phase   │  │  Phase   │  │  Phase   │  │  Phase     │  │ │
│  │  └─────────┘  └──────────┘  └──────────┘  └────────────┘  │ │
│  └─────────────────────────────────────────────────────────────┘ │
│         │                                                        │
│         ▼                                                        │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │                   Cluster State Cache                       │ │
│  │                                                             │ │
│  │  - NodeCapacity per node (from Mustard + reporting tree)    │ │
│  │  - Running workloads per node (from reporting tree)         │ │
│  │  - Namespace quota usage (computed)                         │ │
│  │  - Image availability per node (from Pickle reports)        │ │
│  │  - Node labels (from Mustard gossip)                        │ │
│  └─────────────────────────────────────────────────────────────┘ │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │                Task-Array Leader Loop                       │ │
│  │                                                             │ │
│  │  - Grants chunks of task indices to workers that fit them   │ │
│  │  - Polls workers for progress; one Tick entry per tick      │ │
│  │  - Requeues or holds a lost holder's chunks (§5.2)          │ │
│  └─────────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────┘
```

### Scheduling Algorithm

Meat uses a four-phase placement pipeline for App scheduling:

**Phase 1: Filter.** Eliminate nodes that cannot run the workload.

- Node is not in "state unknown" (has reported since leader election).
- Node has sufficient allocatable CPU, memory, and GPU.
- Node matches all `required` placement labels.
- Node is not cordoned or draining.
- Namespace quota would not be exceeded by this placement.
- For image workloads: node runs containers. An app that names an `image` (or has only a `command`, which runs in a root filesystem), or an init container that names its own image, needs a container runtime (#914). Each node reports `NodeCapabilities::containers`, true when its runtime is runc, Apple Container or the mixed runtime, and false on `--runtime process`, which refuses image apps. A missing capability report counts as false. If no ready node with the labels runs containers, the app is unschedulable with `UnplacedReason::NoContainerRuntime` ("no ready node has a container runtime to run its image"), checked before the other capabilities.
- For process workloads: node has the required binary in its allowlist. Each node reports its `[process_workloads]` policy in its capability report (`NodeCapabilities::process`): whether it runs host processes at all (its runtime can, and `mount_isolation` is off, since a node refuses every host workload under mount isolation until #714 implements it) and its `allowed_binaries`. An `exec` app needs its path there, and a `script` app needs `/bin/sh`. An init container of such an app that names no image runs on the host too, so the first word of its command needs to be there as well (`meat::filter::host_init_binaries`, #935). `meat::filter::host_binary` is the one place that says which binary an app needs, so the node's own refusal and app validation agree with placement. A placement whose node stops allowing the binary moves.
- For runtime knobs: the node allows every one. An app's `[runtime]` capabilities, sysctls and devices (#1122) must all be admitted by the allowlist the node reports (`NodeCapabilities::runtime_knobs`), which is empty unless the node runs rootful runc. Otherwise the app is unplaced with `NoCapableNode`.

The filter checks every node against one `WorkloadRequirements` value built from the spec, and so do daemon placement and the leader's check that a committed placement still holds (`meat::filter::node_can_run`), so the three can't disagree.

**Phase 2: Score.** Rank remaining candidate nodes. Spread comes first: a node running fewer replicas of the App always ranks above one running more, which provides failure-domain diversity. Among nodes running the same number, a weighted score (0-90, higher is better) decides:

- **Bin-packing score (weight 50):** Prefer nodes fuller in both CPU and memory after placing this workload, evenly. This maximises density (see Bin-Packing Algorithm below).
- **Preferred label score (weight 20):** Nodes matching `preferred` labels receive a bonus.
- **Image locality score (weight 15):** Nodes that already have the required image cached score higher (full marks if the image is present, zero otherwise).
- **Node stability score (weight 5):** Prefer nodes with longer uptime, scaled from the node's reported uptime (full marks at 24h).

Spread was once a weight (60, scored on whether a node ran the App at all). That beat bin-packing only against a node with no replica: once every candidate ran one, the busier node took every replacement (#346). The replica count is now the first sort key, so no weight can outvote it.

**Phase 3: Select.** Pick the first node: fewest replicas, then highest score. Remaining ties are broken by node ID (deterministic). For multi-replica placements, Meat runs the pipeline iteratively, updating the cluster state cache after each placement to reflect the newly committed resources.

**Phase 4: Commit.** Write the scheduling decision to the Raft log. Once committed, the decision is replicated to council members and the assignment is sent to the target Bun agent via the reporting tree.

### Data Flow

```
                    Deploy Request
                         │
                         ▼
              ┌─────────────────────┐
              │   Admission Check   │
              │  - Permission valid │
              │  - Quota available  │
              │  - Spec valid       │
              └──────────┬──────────┘
                         │
                         ▼
              ┌─────────────────────┐
              │  Dependency Check   │
              │  - run_before jobs  │
              │    completed?       │
              └──────────┬──────────┘
                         │
                    ┌────┴────┐
                    │         │
                    ▼         ▼
              App Place   Job Allocate
                    │         │
                    ▼         ▼
              ┌─────────────────────┐
              │   Placement Engine  │
              │  Filter → Score →   │
              │  Select → Commit    │
              └──────────┬──────────┘
                         │
                         ▼
              ┌─────────────────────┐
              │   Raft Commit       │
              └──────────┬──────────┘
                         │
                         ▼
              ┌─────────────────────┐
              │   Bun Agent(s)      │
              │   Start workload    │
              └──────────┬──────────┘
                         │
                         ▼
              ┌─────────────────────┐
              │   Report back via   │
              │   hierarchical tree │
              └─────────────────────┘
```

---

## 4. Data Structures

All types are Rust structs using `serde` for serialisation (Raft log entries and the council's Raft RPCs are self-describing JSON, because config types such as `Replicas` need serde's `deserialize_any`; API responses use JSON too).

### Core Scheduling Types

```rust
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Unique identifier for a node in the cluster.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct NodeId(pub String);

/// Unique identifier for an app.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppId {
    pub namespace: String,
    pub name: String,
}

/// Unique identifier for a job.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobId {
    pub namespace: String,
    pub name: String,
}

/// Resource quantities using millicores for CPU and bytes for memory.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Resources {
    /// CPU in millicores (e.g., 500 = 500m = 0.5 cores).
    pub cpu_millis: u32,
    /// Memory in bytes.
    pub memory_bytes: u64,
    /// Whole GPU devices.
    pub gpus: u32,
    /// Disk in bytes (for volume reservations).
    pub disk_bytes: u64,
}

/// A range of resources (min-max), used for app specs.
/// The scheduler reserves `min` during placement and allows burst up to `max`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceRange {
    pub min: Resources,
    pub max: Resources,
}
```

### Placement and Constraints

```rust
/// Placement constraints for an App or Job.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlacementConstraint {
    /// Hard constraints: all labels must match. If no nodes match,
    /// the workload remains unscheduled and the leader records why.
    pub required: BTreeMap<String, String>,

    /// Soft constraints: matching nodes are preferred but non-matching
    /// nodes are acceptable if resources are insufficient.
    pub preferred: BTreeMap<String, String>,
}

/// Replica mode for an App.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ReplicaMode {
    /// Exactly N instances, placed by Meat across available nodes.
    Fixed(u32),
    /// One instance on every node matching placement constraints (daemon mode).
    /// Instances are added/removed as nodes join/leave.
    DaemonSet,
}
```

### App and Job Specs

```rust
/// Workload source: either a container image or a host process.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WorkloadSource {
    /// Container image reference (e.g., "myapp:v1.4.2" or "pickle://api:v1.4.3").
    Image(String),
    /// Host binary path (process workload). Requires host-exec permission.
    Exec {
        binary: Vec<String>,
        args: Vec<String>,
    },
    /// Inline script (process workload). Requires host-exec permission.
    Script(String),
}

/// Deploy configuration for an App (`meat::deploy_types::DeployConfig`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeployConfig {
    /// `Rolling` (replace a few at a time) or `BlueGreen` (stand up the
    /// whole new fleet, then swap).
    pub strategy: DeployStrategy,
    /// Max instances above target during rollout.
    pub max_surge: u32,
    /// Max instances below target during rollout.
    pub max_unavailable: u32,
    /// How long to wait for connections to drain.
    pub drain_timeout: Duration,
    /// How long to wait for health checks after start.
    pub health_timeout: Duration,
    /// Automatically revert on health check failure.
    pub auto_rollback: bool,
}

impl Default for DeployConfig {
    fn default() -> Self {
        Self {
            strategy: DeployStrategy::Rolling,
            max_surge: 1,
            max_unavailable: 0,
            drain_timeout: Duration::from_secs(30),
            health_timeout: Duration::from_secs(60),
            auto_rollback: true,
        }
    }
}

/// Autoscaling configuration for an App (`meat::autoscaler`).
#[derive(Clone, Debug)]
pub struct AutoscaleConfig {
    /// Resource to scale on. Config validation rejects anything but
    /// "cpu" and "memory".
    pub metric: AutoscaleMetric,
    /// Per-replica request in the collector series' unit: percent of one
    /// core for CPU (500m = 50.0; no CPU request = 100.0, one core),
    /// bytes for memory (a memory request is required).
    pub request: f64,
    /// Target utilisation of the request as a fraction (0.70 for "70%").
    pub target: f64,
    /// Minimum replica count, at least 1 (no scale-to-zero). Autoscaler
    /// will never scale below this.
    pub min: u32,
    /// Maximum replica count. Autoscaler will never scale above this.
    pub max: u32,
    // ... evaluation_window, cooldown, scale_down_threshold
}

/// Each variant reads the per-instance series the node collector records,
/// labelled `app = "<namespace>/<app>"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoscaleMetric {
    /// `process_cpu_percent`: percent of ONE core (two busy cores = 200).
    Cpu,
    /// `process_memory_bytes`: resident bytes.
    Memory,
}

/// Init container spec, runs to completion before the main app starts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InitContainer {
    /// Image to use. If None, inherits the parent app's image.
    pub image: Option<String>,
    pub command: Vec<String>,
}

/// Full specification for an App as consumed by the scheduler.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppSpec {
    pub id: AppId,
    pub source: WorkloadSource,
    pub replicas: ReplicaMode,
    pub port: Option<u16>,
    pub health: Option<String>,
    pub resources: ResourceRange,
    pub placement: PlacementConstraint,
    pub deploy: DeployConfig,
    pub autoscale: Option<AutoscaleConfig>,
    pub init_containers: Vec<InitContainer>,
    pub env: BTreeMap<String, String>,
    pub gpu: u32,
    // Dependency ordering lives on the job: `run_before = ["app.NAME"]`.
    /// Spec version, incremented on each change. Used to detect stale state.
    pub spec_version: u64,
}

/// A job's execution template (`config::job::JobSpec`, trimmed). Trigger
/// policy lives outside it, in the definition below.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobSpec {
    /// `runc` (default), `shared-runc` or `process`.
    pub runtime: JobRuntime,
    pub image: Option<String>,
    pub command: Option<Vec<String>>,
    pub exec: Option<PathBuf>,
    pub script: Option<String>,
    /// Cron schedule (UTC), e.g. "0 3 * * *".
    pub schedule: Option<String>,
    /// `["app.NAME"]`: run as a hook before that app publishes.
    pub run_before: Vec<String>,
    pub cpu: Option<ResourceRange>,
    pub memory: Option<ResourceRange>,
    pub namespace: Option<String>,
    /// Attempts per run (1–10). Omitted: 4, or 1 for a hook.
    pub max_attempts: Option<u8>,
    /// Per-attempt limit in seconds; 0 means none. Omitted: none, or 600 for a hook.
    pub task_timeout_secs: Option<u32>,
    /// Cron only: whether an occurrence may start while a run is active.
    pub overlap: Option<OverlapPolicy>,
    /// Replay automatically after an unknown outcome instead of waiting
    /// for `relish batch replay`. Hooks refuse it.
    pub replay_unknown: bool,
    // ... env, pids, firewall, egress
}

/// Reusable work and the policy a trigger uses to create a run (`meat::job`).
pub struct JobDefinition {
    pub template: JobSpec,
    /// Indexed tasks; a TOML job is one task.
    pub tasks: TaskArraySpec,
    /// Durable UTC schedule, if the definition runs periodically.
    pub cron: Option<CronPolicy>,
    pub replay_unknown: bool,
    /// A deployment hook: kept like a schedule, because the next apply
    /// that names it runs it again.
    pub hook: bool,
}

/// Count and execution policy of a task array (`meat::task_array`).
pub struct TaskArraySpec {
    pub count: u32,
    pub chunk_size: u32,
    pub max_attempts: u8,
    pub max_failed_indexes: Option<u32>,
    pub task_timeout_secs: u32,
    pub per_node_concurrency: Option<u32>,
}

/// Stable source identity of one requested run, so a retried request or
/// a re-elected leader can't start the same run twice.
pub enum RunTrigger {
    Manual { request_id: String },
    Hook { operation_id: String },
    Cron { minute: i64 },
}
```

### Node State

```rust
/// A node's capacity and current utilisation, as tracked by the scheduler.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeCapacity {
    pub node_id: NodeId,
    /// Total allocatable resources (total minus reserved).
    pub allocatable: Resources,
    /// Currently allocated to running workloads.
    pub allocated: Resources,
    /// Labels from node.toml (zone, gpu_model, storage, role, etc.).
    pub labels: BTreeMap<String, String>,
    /// Whether the node is ready to accept workloads.
    pub status: NodeStatus,
    /// Timestamp of the last state report received.
    pub last_report: Instant,
    /// Set of image references available in the local Pickle cache.
    pub cached_images: HashSet<String>,
    /// Node uptime since last Bun restart.
    pub uptime: Duration,
    /// Whether process workloads are enabled (allowlist configured).
    pub process_workloads_enabled: bool,
    /// Allowed binaries for process workloads.
    pub allowed_binaries: Vec<String>,
    /// Number of GPU devices available (detected via `/dev/nvidia0` + `nvidia-smi`).
    pub gpu_count: u32,
    /// GPU model string (e.g., "a100", "h100"). Auto-detected.
    pub gpu_model: Option<String>,
    /// Available host ports in the configured range.
    pub available_port_count: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum NodeStatus {
    /// Node has reported and is ready to accept workloads.
    Ready,
    /// Node has not reported since leader election. No new work scheduled.
    Unknown,
    /// Node is being drained (existing workloads migrate out, no new placements).
    Draining,
    /// Node is cordoned (no new placements, existing workloads remain).
    Cordoned,
    /// Node is unreachable (detected by Mustard gossip failure detection).
    Unreachable,
}
```

### Scheduling Decisions

The shipped `SchedulingDecision` (in `meat::types`) is deliberately minimal: it
carries the app being scheduled and one `Placement` (node, reserved resources
and the replica's ordinal) per replica. Instance ordinals are covered in
§5.1. The richer shape below — a per-decision `decision_id`, a
`spec_version`, an allocated port, and a timestamp — is **planned — not yet
implemented**; those fields back the replay-protection design in Section 8.4,
which the current struct cannot enforce.

```rust
/// A placement decision for a single App replica. (Planned shape.)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchedulingDecision {
    pub decision_id: u64,
    pub app_id: AppId,
    pub target_node: NodeId,
    /// The host port allocated on the target node.
    pub assigned_port: Option<u16>,
    /// Resources reserved on the target node.
    pub reserved_resources: Resources,
    /// Timestamp of the decision.
    pub timestamp: Instant,
    /// Spec version this decision was made against.
    pub spec_version: u64,
}

```

Jobs have no per-node allocation decision. The leader's `TaskArrayWrite`
entries (`Register`, `Sync` with chunk results and new grants, `Requeue`,
`Unknown`, `Replay`, `Cancel`, batched into one `Tick` per leader tick) carry
grants and outcomes; the appendix describes them.

```

### Namespace Quotas

```rust
/// Namespace resource quota, enforced at scheduling time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NamespaceQuota {
    pub namespace: String,
    /// Total CPU budget in millicores.
    pub cpu_millis: Option<u32>,
    /// Total memory budget in bytes.
    pub memory_bytes: Option<u64>,
    /// Total GPU budget (whole devices).
    pub gpus: Option<u32>,
    /// Maximum number of apps.
    pub max_apps: Option<u32>,
    /// Maximum total replicas across all apps.
    pub max_replicas: Option<u32>,
}

/// Tracked usage for a namespace, updated after each scheduling decision.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NamespaceUsage {
    pub namespace: String,
    pub cpu_millis_allocated: u32,
    pub memory_bytes_allocated: u64,
    pub gpus_allocated: u32,
    pub app_count: u32,
    pub replica_count: u32,
}
```

### Rolling Deploy State

There is no rollout state machine in Raft. Each node rolls its own share with
the pure planner `meat::deploy_types::plan_rolling_step`, and the leader's
record of an app's rollouts is small:

```rust
/// What a rolling replacement should do next.
pub enum RollingStep {
    StartNew,
    RetireOld,
    /// Surge is exhausted by instances that haven't gone healthy yet.
    Wait,
    Done,
    /// Neither move is allowed; only reachable with both bounds at zero,
    /// which validation refuses.
    Stuck,
}

/// The council's per-app rollout record (`meat::deploy_types::AppRollouts`):
/// the last specs that rolled out on every node, and the spec whose rollout
/// failed, by fingerprint.
pub struct AppRollouts {
    pub released: Vec<AppSpec>,
    pub failed: Option<RolloutFailure>,
}
```

### Autoscaler State

```rust
/// Runtime state for the autoscaler for a single App.
#[derive(Clone, Debug)]
pub struct AutoscaleState {
    pub app_id: AppId,
    pub config: AutoscaleConfig,
    /// The replica count set by the autoscaler (overrides spec.replicas).
    pub current_replicas: u32,
    /// Base replica count from the spec (used as the lower reference point).
    pub base_replicas: u32,
    /// Timestamp of the last scale event, for cooldown enforcement.
    pub last_scale_event: Option<Instant>,
    /// Rolling window of metric samples for smoothing.
    pub metric_samples: VecDeque<MetricSample>,
}

#[derive(Clone, Debug)]
pub struct MetricSample {
    pub value: f64,
    pub timestamp: Instant,
}
```

---

## 5. Operations

### 5.1 App Placement

#### Fixed Replica Mode (`replicas = N`)

When an App spec with `replicas = 3` is submitted:

1. The API validates the spec (schema, permissions) before committing it; Meat checks the namespace quota in its scheduling pass (§5.6).
2. For each replica (0..3), Meat runs the four-phase placement pipeline:
   - **Filter:** Eliminate nodes that lack resources, do not match `required` labels, or are not ready.
   - **Score:** Rank candidates by fewest replicas of the App, then by the weighted score (bin-packing 50, preferred labels 20, image locality 15, stability 5).
   - **Select:** Pick the highest-scoring node.
   - **Commit:** Reserve resources on the selected node in the cluster state cache, then commit the `SchedulingDecision` to the Raft log.
3. After all replicas are committed, the decisions are disseminated to target Bun agents.

The iterative per-replica approach (rather than computing all placements at once) ensures that spread accumulates correctly: the second replica of an App ranks a given node lower if the first replica was already placed there.

#### Short Capacity

When only some replicas fit, Meat places those and leaves the rest unplaced, as Kubernetes does (D29). Until 0.2.1 a fixed-size app got none of its missing replicas when any one of them didn't fit, with nothing recorded but a log line every tick.

- **What's placed stays placed.** Each replica that found a node is committed and keeps its reservation in the pass's cache, so the apps planned after it see it.
- **Nothing moves without somewhere to go.** A committed replica whose node no longer holds it (not ready, gone, lapsed) but that no other node can take stays committed where it was, and isn't counted as placed. Dropping it would retire a workload that may still be running and replace it with nothing; the self-upgrade revert test caught exactly that on a lone node that briefly reported not ready while restarting. It moves as soon as a node can take it.
- **The reason is durable.** The pass returns, for every short app, how many replicas it placed, how many it wants, and the first hard constraint no node met (`UnplacedReason`, checked in filter order): no ready node, no node with the `required` labels, no node that runs containers for an image app, no node that can enforce the app's egress or DNS needs or run its host binary, or no room. The leader proposes `RaftRequest::Unschedulable`, which replaces `DesiredState::unschedulable`, only when that set changes (`unschedulable_update`), like `QuotaBlocked`. Every node serves it in its desired-app evidence (`GET /v1/diagnostics/apps`, field `unschedulable`):
  ```
  $ relish status
  web (namespace prod) is unschedulable: placed 2 of 3 replicas: no node that can run it has room
  ```
  `relish wtf` raises an `unschedulable` warning with the same reason in place of the generic under-replication.
- **Logged once per change.** The leader logs `scheduler: cannot place every replica of …` when an app becomes short or its count or reason changes, and `every replica of … is placed` when it clears, and records a `placement` event when it starts and when it clears. A steady short app writes nothing to the Raft log: the pass skips a decision that repeats the committed placements.
- **Clearing.** When a node joins, frees room or gains the label or capability, the next pass places the rest and the reason goes. Deleting the app removes it in the same Raft entry.
- **Not covered.** A daemon set wants exactly the nodes that can run it, so it is never short. An app its namespace quota blocks is reported as `quota_blocked` instead (§5.6). There is no Mayo alert for a short app yet; the event and `relish wtf` are how it shows.

#### Required Labels (Hard Constraints)

```toml
[app.inference.placement]
required = { zone = "us-east-1", gpu_model = "a100" }
```

During the Filter phase, every node's `labels` map is checked. A node passes the filter only if for every key-value pair in `required`, the node has that exact label. This is a logical AND across all required labels. If zero nodes pass the filter, the replicas stay unplaced and the leader records why (see Short Capacity below).

#### Preferred Labels (Soft Constraints)

```toml
[app.api.placement]
preferred = { storage = "ssd" }
```

Preferred labels do not eliminate nodes in the Filter phase. Instead, during Score, nodes matching preferred labels receive a bonus (up to 20 points). The bonus is proportional to how many preferred labels match. A node matching all preferred labels gets 20 points; a node matching half gets 10.

When both `required` and `preferred` are specified, `required` is enforced first (Filter), then `preferred` is used to rank the remaining candidates (Score).

#### Bin-Packing Algorithm

The bin-packing score for a candidate node is calculated as:

```rust
fn bin_packing_score(node: &SchedulerNodeState, resources: &Resources) -> u32 {
    let cpu = utilisation_after(node.allocated.cpu_millicores, resources.cpu_millicores,
                                node.allocatable.cpu_millicores);
    let memory = utilisation_after(node.allocated.memory_bytes, resources.memory_bytes,
                                   node.allocatable.memory_bytes);
    let score = match (cpu, memory) {
        (Some(cpu), Some(memory)) => {
            let average = (cpu + memory) / 2;
            average.saturating_sub(cpu.abs_diff(memory) / 10)
        }
        (Some(only), None) | (None, Some(only)) => only,
        (None, None) => 50,
    };
    u32::try_from(score).unwrap_or(100)
}
```

`utilisation_after` is the percentage of the allocatable resource in use once the request lands, capped at 100. Higher utilisation after placement is better (the node is more "full"). The score is the average of CPU and memory utilisation, less a tenth of the gap between them: an imbalance penalty discourages placing CPU-heavy workloads on memory-heavy nodes and vice versa, which would leave stranded resources. The 0-100 result is weighted 50 in the total. It works in integer percentages so the score, and so the tiebreak, is the same on every platform. Until 0.2.1 (D30) the score used CPU alone, so a memory-heavy app spread over half-empty nodes rather than packing onto the one with memory to spare.

#### Apps with Managed Volumes

A managed volume lives on one node, so placement follows the data. Desired state keeps `last_placed_nodes`: the nodes of the app's last non-empty scheduling decision. `relish stop` commits an empty decision, which leaves that record alone. When a fixed-replica app with a managed volume has no placement left to keep (it was stopped and is applied again), the leader puts its replicas back on those nodes before scoring anything:

- A home node that is alive, ready and still matches the app's required labels gets the replica, reserved in the pass's cache.
- A home node that is alive but can't take it right now makes the app wait: placing it elsewhere would start it on an empty volume. That covers no room, no fresh report (a new leader, or a report worker that stalled for longer than `stale_report_timeout_secs`), not ready, cordoned for an upgrade, and missing a capability the app needs.
- A home node that is out of the cluster (Suspect, Left, Dead, or reaped from gossip) also makes the app wait. A `systemctl restart bun` announces Left, and a reboot goes Suspect then Dead, with the data still on disk (#423).
- A home node that was decommissioned (`relish decommission-node`), or that no longer matches the labels, is dropped, and that replica goes through the normal pipeline. Both are the operator moving the app on purpose, and the app starts on a new, empty volume.

The same rule keeps a *running* volume app where it is. For an app without a volume, a placement on a node that reported not ready, was cordoned or left the cluster is replaced elsewhere, and so is one on a node whose reports went stale, once the stale-node grace runs out (§7.5). For a fixed-replica app with a managed volume (`keeps_volume_home`), the placement holds whatever the node is doing until the node is decommissioned or the app's `required` labels stop matching it, including for each replica of a multi-replica app. Moving it would restart it on an empty volume while its data sits on the node it left.

While an app waits for a home that is out of the cluster, the council's per-app evidence (`GET /v1/diagnostics/apps`) carries `volume_home_away`, the node it waits for. `relish status`, `relish inspect` and the dashboard show it, and `relish wtf` reports a critical `volume-home-away` finding naming the node and the decommission command.

`relish delete` forgets the record. Apps without a managed volume are placed by score as usual.

#### Changing a Running App's Spec

A new spec for a running app keeps its committed placements. Each node agent rolls its own replicas to the new spec (rolling or blue-green), so dropping every placement on a resource change would retire all the replicas at once. Instead, every pass revalidates each committed placement against the spec as it is now (#434):

- **A changed hard selector moves the replica** when its node no longer matches `required`. It's the one spec change that moves an app, and it moves a managed-volume app too: the operator asked for other nodes, and the app starts there on an empty volume. A node that still matches keeps its replicas.
- **A changed resource request is admitted where the replica runs.** The node credits back the footprint the placement was committed with and checks the new request against what's left, because the new replica replaces the old one rather than running beside it. If it fits, the replica stays and the decision records the new footprint. If it doesn't, a replica without a managed volume is placed elsewhere; a managed-volume app stays on its home whatever its new request (its data is there), and runs over its node's budget until someone makes room.

There is a window of one scheduling tick: a node agent can roll to the new spec before the leader's next pass decides that the replica must move. The pass then places it elsewhere and the agent retires it.

#### Instance Ordinals

An instance is named `{namespace}__{app}-{ordinal}`, or
`{namespace}__{app}-g{generation}-{ordinal}` once a rolling or blue-green deploy
has replaced it. The leader picks the ordinal, not the node: each `Placement`
carries one, distinct within the app across the whole cluster, so three
replicas on three nodes are `frontend-0`, `frontend-1` and `frontend-2`. Until
#398 every node counted its own replicas from 0 and all three were
`frontend-0`. The council refuses a decision in which two placements share an
ordinal.

Ordinals stay put. Every pass numbers an app's placements with
`number_placements`:

- A placement the pass keeps keeps its ordinal.
- A fixed-replica app that shrinks keeps its lowest ordinals, so a scale-down
  retires the highest.
- A new placement takes the lowest free ordinal. After a node loss that's the
  ordinal the lost replica had, so the replacement takes over its name on
  another node; on a scale-up it's the next ones up.
- A daemon set is re-placed whole when its eligible nodes change, so a new
  placement first takes back the ordinal its node held before. A node keeps
  its daemon's number while it stays eligible, and a joining node takes the
  lowest free one. We considered naming a daemon's instance after its node
  instead, but cluster-wide ordinals keep one naming rule for every app, and
  the node is already in every place an instance is shown.
- A managed-volume app returning from `relish stop` goes back to its home
  nodes in the order `last_placed_nodes` records them, which is ordinal
  order, so each home gets its old ordinal back.

`/v1/placements/{node}` hands each node its ordinals for an app
(`NodeAssignment::ordinals`), and the placement reconciler passes them to the
agent in the app spec's `ordinals` field, which a config file may not set. The
agent creates exactly those instances. A share that grows starts only the new
ordinals beside the running ones, and a rolling deploy starts generation
`g{n}` under the same ordinals. A standalone node, with no leader, numbers its
replicas from 0.

#### Reservations between Reports

Node reports lag the council. A placement committed a tick ago isn't in its node's report yet, so the pass that starts from the reports would see free room and place a second app into it. Before admitting anything, each pass rebuilds the missing reservations: for every committed placement beyond the replicas its node reports, it reserves the committed footprint in the pass's cache (#432).

#### Daemon Mode (`replicas = "*"`)

When `replicas = "*"` is specified, Meat does not run the placement pipeline. Instead:

1. It queries all nodes from the cluster state cache.
2. If placement `required` labels are specified, it filters to matching nodes.
3. One instance is scheduled on every qualifying node, with a cluster-wide
   ordinal like any other replica (see Instance Ordinals).
4. Meat subscribes to Mustard membership events: when a new node joins the cluster and matches the placement constraints, an instance is automatically scheduled. When a node leaves, the instance is removed.

Daemon mode is not bin-packed: every qualifying node that has room gets exactly one instance. "Qualifying" is the same `filter_nodes` check fixed replicas pass (`daemon_candidates` in `src/meat/scheduler.rs`), so a node without room for the request, or one that can't run the workload, gets none. This is appropriate for system-level workloads (node exporters, log forwarders, caches).

A node already running the daemon's committed copy doesn't need room for a second one. Eligibility credits that copy back at the footprint it was committed with, in a throwaway view of the cache, and the pass trades it for the new request; the real cache keeps the allocation, so no other app can spend the room (#433). A copy the node runs without a committed placement earns no credit.

### 5.2 Jobs: the Common Lifecycle

Every job takes one path, whatever started it: a `[job.NAME]` table in an
apply, a `relish batch` array or manifest, `relish run --batch`, a cron
occurrence or a `run_before` hook. The types are in `src/meat/job.rs`,
`src/meat/task_array.rs` and `src/meat/task_array_store.rs`; the leader loop
is `src/bun/task_array_leader.rs`.

- **Definitions.** A `JobDefinition` holds the execution template (the
  `JobSpec`), a `TaskArraySpec` (a TOML job is one task), an optional
  `CronPolicy` and the `hook` flag. Definitions are revisioned. One-off
  definitions expire with their last run; schedules and hooks last, because
  the next minute or the next apply runs them again.
- **Runs.** A trigger creates a run. `RunTrigger` gives each run a stable
  identity (a manual request id, a hook's deployment operation id, or a
  definition revision plus UTC minute), so a retried request or a new
  leader returns the existing run instead of starting a second.
- **Chunks.** The leader hands out chunks of task indices to workers that
  report how many requests fit, and a node-wide `ExecutionBudget` reserves the
  CPU and memory each attempt needs. Chunk size is queue depth, not
  simultaneous resource use. Workers stream task outcomes into checksummed
  ledgers and return finished chunks fenced by their grant attempt. The
  leader commits one `Tick` entry per tick with every array's progress, so
  the Raft log grows with chunks, not tasks.
- **Unknown outcomes.** A holder that goes quiet past the silence window, or
  refuses the array, loses its chunks. With `replay_unknown = true` the
  leader requeues them (`TaskArrayWrite::Requeue`) at the next grant attempt.
  Without it, which is the default, the leader records the node as an
  unknown owner (`TaskArrayWrite::Unknown`) and keeps its tasks off every
  other node: they may have run, and a second run could repeat their side
  effects. An operator who has checked acknowledges the exact grant with
  `relish batch replay ID --node NODE --grant-digest DIGEST
  --acknowledge-side-effects`, which writes `Replay`. Without
  `replay_unknown`, an attempt that times out or ends `Unknown` on a live
  worker is treated the same way: the task isn't retried.
- **Cron.** The leader observes each UTC minute once (`CronObserve`) and
  claims every matching minute of every schedule as one run. `overlap =
  "forbid"` (the default) skips an occurrence while a run is active; `allow`
  starts another. Minutes missed while no leader was available are skipped,
  not replayed (`MissedRunPolicy::Skip`). An occurrence with no active-run
  slot free is recorded as skipped for capacity, and while a definition has
  an unknown owner its occurrences are skipped too, whatever `overlap` says.
- **Hooks.** An apply with `run_before` jobs writes `DeployBegin`, which
  persists the deployment intent and admits only the hooks. When every hook
  has an accepted successful outcome, `DeployCommitted` publishes the apps
  and admits the ordinary jobs and schedules in one entry. A failed hook
  fails the deploy before any instance changes; an unknown one never opens
  the gate.

Execution is at least once, so external effects need business idempotency
keys. Limits (active runs, definitions per namespace, retained receipts) are
council-enforced; the [manual](../manual/14_batch-jobs.md) lists them, and
§5.6 covers quotas. The [appendix](#delegated-task-array-implementation-020)
has the details of grants, budgets, ledgers and recovery.

> **What this section used to say.** The Phase 12 design here partitioned a
> batch's `parallelism` across nodes with a `BatchAllocation` per node,
> re-allocated failed instances to other nodes, and ran `replicas = "*"` cron
> jobs on every node. The pieces that approximated it (the
> `meat::batch::schedule_batch` allocator, the leader's `BatchTracker` with
> its `BatchRegister` reservations, node-to-node `/v1/batch/run` dispatch)
> were retired in 0.2.0 (#681). Re-allocating a failed task to another node
> is exactly what the unknown-outcome rule forbids; there is no
> `parallelism` field and no daemon cron.

### 5.3 Rolling Deploys

Meat doesn't drive a rollout instance by instance. Each node rolls its own
share of an app, and the leader decides which nodes may start a new version.

**On a node.** The deploy worker (`rolling_redeploy` in
`src/bun/agent/deploy_worker.rs`) asks the pure planner
`meat::deploy_types::plan_rolling_step` what to do next, acts, and asks
again until it answers `Done`:

1. `StartNew`: start a replacement, wait for its health check (up to
   `health_timeout`) and publish it as a backend.
2. `RetireOld`: drain an old instance (up to `drain_timeout`) and stop it.

`max_surge` caps how far above the target the total may go, which decides
whether another replacement may start; `max_unavailable` caps how far below
it the serving count may fall, which decides whether an old instance may
retire. The defaults (1 and 0) replace one instance at a time without ever
dipping. A managed-volume app rolls stop-first, because two processes must
never write one directory. `strategy = "blue-green"` dispatches to
`blue_green_redeploy` instead: the whole new fleet comes up healthy beside the
old one, is published, and then the old fleet retires.

If a replacement fails its health check, `abort_rollout` decides:

- `auto_rollback = false`: keep the healthy replacements, record `Halted`.
- `auto_rollback = true` (the default) and nothing retired yet: tear the
  replacements down; the old fleet carries on.
- `auto_rollback = true` after a retirement: roll backwards with the same
  planner, starting previous-spec instances before retiring a healthy
  replacement, so an abort never ends below the capacity it started with.

**Across the cluster.** The leader releases a spec change to one node at a
time (`plan_release` in `src/cluster/rollout.rs`) and waits for that node's
share to report healthy before the next. Progress lives in the nodes' state
reports, not in Raft, so a new leader carries on where the old one stopped. A
held node keeps the spec it runs. Versions are compared by
`rollout_fingerprint`, which sets `replicas` and ordinals aside, so a scale
change is never mistaken for a rollout.

When a node aborts, it reports the failure and the leader commits
`RolloutFailed` with the fingerprint, node and reason. From then on that spec
goes to nobody who doesn't already run it, until the operator applies a
different spec. When every node runs the current spec healthy, the leader
commits `RolloutReleased`, which keeps the last ten distinct specs that rolled
out everywhere: what held nodes fall back on, and where `relish rollback`
looks first.

Full leader-coordinated rollouts, with one surge budget across the cluster and
the rollout's progress in Raft, remain a possible future design (decision D10
in #674).

#### Dependency Ordering

Jobs with `run_before = ["app.api"]` are hooks. They run on the common job
lifecycle before the app publishes (§5.2): the apply admits the hooks, waits
for their accepted successful outcomes, and only then publishes the app and
admits the ordinary jobs in one Raft entry. A failed hook fails the deploy.
Each hook names an app in the same apply and namespace; there is no DAG of
hooks depending on hooks.

### 5.4 Autoscaling Logic

The autoscaler runs as a periodic task within Meat (default interval: 30 seconds).

#### Metric semantics

Utilisation follows the Kubernetes HPA convention: the average per-replica use divided by the per-replica **request**. The leader reads the rollup store for the metric's collector series (`process_cpu_percent` or `process_memory_bytes`), filtered to the app's exact `namespace/app` label, over `evaluation_window`; averages the per-instance, per-minute means; and divides by the request. `target = "50%"` on `cpu` with `cpu = "500m-1000m"` therefore means "a quarter of a core per replica on average".

- **CPU with no CPU request** is measured against one whole core. ProcessGrill and rootless nodes refuse apps that declare `cpu` (they can't enforce the limit), so refusing here would make CPU autoscaling impossible on them.
- **Memory with no memory request** fails config validation: there's no natural unit to fall back on.
- **Any other `metric`** fails config validation. Custom (scraped) metrics are not supported.
- **`min = 0`** fails config validation. Scale-to-zero needs a wake-up signal that exists while no replica runs (requests queued at the ingress, say), and both metrics are sampled from running instances: an app at zero replicas reports nothing to scale back up on. `min` must be at least 1; `relish stop` parks an app at zero by hand.
- **Limits.** The collector samples each instance's main process (children aren't counted), and only runtimes that report a host PID produce per-app metrics: ProcessGrill and runc do; direct Apple Container (VM-isolated, disabled in 0.1.0) doesn't, so its apps can't autoscale. Rollups cover the previous complete minute, so the first signal reaches the leader 60–120 s after load changes.

#### Algorithm

```rust
impl AutoscaleState {
    fn evaluate(&mut self, cluster_state: &ClusterStateCache) -> Option<ScaleAction> {
        // 1. Collect current metric value (average across all instances).
        let current_value = self.compute_current_metric(cluster_state)?;

        // 2. Add to rolling window.
        self.metric_samples.push_back(MetricSample {
            value: current_value,
            timestamp: Instant::now(),
        });

        // 3. Trim samples older than the stabilization window (default 5 min).
        let window = Duration::from_secs(300);
        while self.metric_samples.front()
            .map_or(false, |s| s.timestamp.elapsed() > window)
        {
            self.metric_samples.pop_front();
        }

        // 4. Compute smoothed metric (average over the window).
        let smoothed: f64 = self.metric_samples.iter()
            .map(|s| s.value)
            .sum::<f64>() / self.metric_samples.len() as f64;

        // 5. Compute desired replicas.
        let desired = ((smoothed / self.config.target as f64)
            * self.current_replicas as f64)
            .ceil() as u32;
        let clamped = desired.clamp(self.config.min, self.config.max);

        // 6. Enforce cooldown (default: 60s for scale-up, 300s for scale-down).
        if clamped != self.current_replicas {
            if let Some(last) = self.last_scale_event {
                let cooldown = if clamped > self.current_replicas {
                    Duration::from_secs(60)  // scale-up cooldown
                } else {
                    Duration::from_secs(300) // scale-down cooldown (conservative)
                };
                if last.elapsed() < cooldown {
                    return None; // Still in cooldown period.
                }
            }
        }

        if clamped > self.current_replicas {
            Some(ScaleAction::ScaleUp {
                from: self.current_replicas,
                to: clamped,
            })
        } else if clamped < self.current_replicas {
            Some(ScaleAction::ScaleDown {
                from: self.current_replicas,
                to: clamped,
            })
        } else {
            None
        }
    }
}

enum ScaleAction {
    ScaleUp { from: u32, to: u32 },
    ScaleDown { from: u32, to: u32 },
}
```

When a scale action is determined:

- **Scale up:** Meat runs the placement pipeline for the additional replicas, identical to initial placement.
- **Scale down:** Meat retires the replicas with the highest ordinals, wherever they run, so the surviving replicas keep ordinals 0 to N-1 (`a_scale_down_retires_the_highest_ordinals` in `src/cluster/orchestrate.rs`). Removed instances go through the drain flow (Wrapper removes them from routing, waits for `drain_timeout`, stops them).

#### Interaction with GitOps (Lettuce)

Lettuce treats the `replicas` field in git as the *base* replica count. Autoscaler adjustments are stored as runtime overrides in Raft. When Lettuce processes a git change:

- If the `replicas` field itself changed in git, the runtime override is reset and the new base value takes effect.
- If only other fields changed (image, env, resources), the autoscaler's current replica count is preserved.

This prevents Lettuce from fighting the autoscaler during traffic spikes.

### 5.5 Daemon Mode Reconciliation

For `replicas = "*"` apps, Meat subscribes to Mustard membership events and runs a reconciliation loop:

```rust
fn reconcile_daemon_apps(&self, membership_event: MembershipEvent) {
    for app in self.daemon_apps() {
        let qualifying_nodes = self.nodes_matching(&app.placement.required);
        let current_instances = self.instances_of(&app.id);

        // Nodes that should have an instance but don't.
        let missing: Vec<_> = qualifying_nodes.iter()
            .filter(|n| !current_instances.iter().any(|i| i.node_id == n.node_id))
            .collect();

        // Nodes that have an instance but no longer qualify.
        let excess: Vec<_> = current_instances.iter()
            .filter(|i| !qualifying_nodes.iter().any(|n| n.node_id == i.node_id))
            .collect();

        for node in missing {
            self.schedule_instance(&app, &node.node_id);
        }
        for instance in excess {
            self.stop_instance(&instance.instance_id);
        }
    }
}
```

In the code a daemon set's target is the eligible nodes plus the copies it holds on nodes the leader can't judge yet: a suspect node, a live node whose readiness hasn't arrived, and a stale node within its grace (§7.5). Those copies stay; once the node is judged gone or lapses, its copy is dropped.

### 5.6 Namespace Quota Enforcement

Quotas are enforced at scheduling time, not at apply time and not retroactively. `relish apply` commits an over-quota app to desired state like any other; the leader's scheduling pass (`plan_scheduling_pass` in `src/cluster/orchestrate.rs`) seeds a `QuotaLedger` with every converged app's footprint, then runs this check before placing each unconverged app. An app that fails it gets no placement that pass, and the pass returns the `QuotaError` beside its placements (`PassPlan::quota_blocked`). The leader keeps those reasons in council state, `DesiredState::quota_blocked`, so any node can say why an app isn't running (see §7.3). The check runs again every pass, so the app places once the namespace has room, and its reason clears in the same pass. `over_quota_app_is_not_placed_and_the_pass_says_why` pins this contract. The check, in outline:

```rust
fn check_namespace_quota(
    &self,
    spec: &AppSpec,
    usage: &NamespaceUsage,
    quota: &NamespaceQuota,
) -> Result<(), QuotaError> {
    if let Some(max_apps) = quota.max_apps {
        if usage.app_count + 1 > max_apps {
            return Err(QuotaError::MaxAppsExceeded {
                namespace: quota.namespace.clone(),
                current: usage.app_count,
                limit: max_apps,
            });
        }
    }

    let new_replicas = match &spec.replicas {
        ReplicaMode::Fixed(n) => *n,
        ReplicaMode::DaemonSet => self.qualifying_node_count(&spec.placement),
    };

    if let Some(max_replicas) = quota.max_replicas {
        if usage.replica_count + new_replicas > max_replicas {
            return Err(QuotaError::MaxReplicasExceeded {
                namespace: quota.namespace.clone(),
                current: usage.replica_count,
                requested: new_replicas,
                limit: max_replicas,
            });
        }
    }

    let total_cpu = spec.resources.min.cpu_millis * new_replicas;
    if let Some(cpu_limit) = quota.cpu_millis {
        if usage.cpu_millis_allocated + total_cpu > cpu_limit {
            return Err(QuotaError::CpuExceeded {
                namespace: quota.namespace.clone(),
                current_millis: usage.cpu_millis_allocated,
                requested_millis: total_cpu,
                limit_millis: cpu_limit,
            });
        }
    }

    // Similar checks for memory, GPU...
    Ok(())
}
```

The `default` namespace has no quotas unless explicitly configured, which is appropriate for single-team clusters. Multi-team clusters should configure per-team namespaces with quotas from day one.

#### Jobs: quotas at grant time and admission caps

Jobs never pass through the app placement pass: every job is a task array, and the task-array leader (`leader_tick` in `src/bun/task_array_leader.rs`) hands out chunks once a second. So that is where job quotas are enforced (decision D20). Each tick, `GrantBudget::new` builds one `QuotaLedger` per namespace from `DesiredState::namespaces`, charges every app placement's reserved resources first, then every job attempt already in flight, and only then plans new grants, oldest run first. An attempt in flight on a node is `min(tasks it holds, its slots for that run)` (`TaskArrayState::in_flight_on`), because a node never runs more of an array at once than its slots. `fit_grants` trims each run's planned grants to the room that's left; a chunk that would start more attempts than fit stops the run's grants for that tick. Deeper chunks on a node already running a full round cost nothing extra, so lookahead survives.

A run whose grants were held back gets a durable `quota_blocked` reason through a `TaskArrayWrite::QuotaBlocked` entry, written only when the limit it names changes (`same_limit` ignores the usage numbers, which move with every task). It clears when grants fit again or the run finishes. Running attempts are never cancelled. Apps go first in the ledger and the app pass doesn't count jobs, so a namespace's jobs can never displace an app that fits its quota. `max_apps` and `max_replicas` stay app-only.

Admission caps sit in the replicated store (`src/meat/task_array_store.rs`, decision D21):

| Constant | Value | Applies to |
|----------|-------|------------|
| `MAX_ACTIVE_ARRAYS` | 128 | every active run |
| `RESERVED_TRIGGERED_RUNS` | 32 | kept for cron fires and deployment hooks; bulk stops at 96 |
| `MAX_ACTIVE_RUNS_PER_NAMESPACE` | 64 | active arrays, profiles, finite batches and manual runs in one namespace |
| `MAX_JOB_DEFINITIONS_PER_NAMESPACE` | 64 | job definitions in one namespace |
| `MAX_JOB_DEFINITIONS` | 1,024 | job definitions in the cluster |

A run's trigger decides its class: `Cron` and `Hook` triggers are *triggered*, everything else is *bulk*. A cron `Fire` refused even with the headroom is recorded rather than failed: the cursor advances, the definition keeps `skipped: {minute, reason: capacity}` and a count, and the leader emits a `job-skipped` event. `control_state_stays_within_budget_at_the_global_cap` justifies 128: at the cap, with the largest template and saturated failure ranges, job state stays under 32 MiB (half a Raft frame) and one node's sync call under its 8 MiB body limit. 256 would not.

Definitions with no cron and no hook expire in the same deterministic `prune` that drops their last run (D22), unless an unsettled deployment still owns the name. The cron loop takes at most sixteen due schedules per tick and starts each tick after the last one it fired, so a long list takes turns. It reads the minute from the service's `CronClock`: the UTC wall clock in production, a shared manual clock in tests (`TaskArrayService::with_cron_clock`), so a test decides exactly when an occurrence comes round instead of waiting for a real minute (#748). Only cron firing uses it; registration baselines, retention and run timestamps stay on the wall clock.

#### The task-array leader loop at scale

Each tick (`SYNC_INTERVAL`, one second) the leader:

1. Reads the job control state in place with `read_control`, a closure run under the state machine's read lock, and builds each node's sync request. It never clones the whole `DesiredState`; `read_task_arrays` copies only `TaskArrays`, and only for callers that need an owned copy.
2. Calls every node that isn't still answering an earlier call, and gathers answers until the tick's deadline, `sync_deadline(interval)` (half the interval). Calls that miss it stay in `SyncCalls` and are collected by a later tick; their node isn't called again meanwhile, and `NODE_SYNC_TIMEOUT` (3 s for the request, 3 s for the body) still bounds each one. A node that missed the deadline keeps its chunks and counts towards `SILENCE_TIMEOUT` (30 s) like any silent node, so a hung or slow node delays only its own grants.
3. Plans every running array with `plan_tick`, again under the read lock: a lost holder's `Requeue` (or `Unknown`, for runs that need acknowledged replay), then the array's `Sync`, then its `QuotaBlocked` note when the limit changed. Planning copies an array's state only when a chunk finished (`Cow`).
4. Commits the whole batch as one `TaskArrayWrite::Tick` entry (`tick_entries`; at most `MAX_TICK_WRITES` = 1,024 writes per entry, so more would split). The state machine refuses a `Tick` holding anything but those four progress writes, and otherwise applies each on its own: a refused write (an array pruned since planning, a stale `Unknown`) leaves its array unchanged and is reported by array in `TaskArrayApplied::Ticked` / `CouncilResponse::TaskArrayTicked`; the others apply.

The dispatch loop therefore writes at most one Raft entry per tick, however many arrays run, plus a `CronObserve` once a minute and one entry per cron fire. The 0.2.0 budget (#668, D18) is at most three entries a second. `make bench-task-arrays` reports the tick's cost in the `leader_tick` group, with 64 arrays on three nodes (about 1.1 ms of planning and applying on the development laptop) and with one array of one-task chunks on ten nodes (about 160 µs).

**Accepted-range bound.** `TaskArrayState` records the grant (`node → attempt → chunks`) whose report retired each chunk, so results and logs can be fetched from the node that ran it. One-task chunks finishing out of order across many nodes split that record into a range per chunk: 65,536 chunks on ten nodes reached 875 KiB of replicated state. Past `MAX_ACCEPTED_RANGES` (4,096) ranges the per-node record folds into `accepted_folded` (`attempt → chunks`), where the same chunks merge into a few ranges; the worst shape stays under 60 KiB. Folding drops the owner but keeps the attempt, which still fences a result row on its own because each attempt of a chunk is granted to exactly one node. `accepted_grant` returns `owner: None` for a folded chunk, and the results and logs endpoints then ask every node for that grant.

---

## 6. Configuration

> **Status: planned — not yet implemented.** The `[scheduler]` (and
> `[defaults.app.deploy]`) configuration namespace described in this section does
> not exist yet. The scheduler's behaviour is currently baked into constants:
> the reconcile tick is a hardcoded 2 seconds, the scoring weights are `const`s
> in `meat::score` (bin-pack 50, preferred 20, image 15, stability 5; spread is
> the first sort key, not a weight),
> and there is no runtime knob to change any of them. Treat the tables below as
> the intended configuration surface, not as keys you can set today. (The
> per-node reconstruction knobs under `[reconstruction]` — e.g.
> `learning_period_timeout_secs` — do exist; the `scheduler.*` keys do not.)

All scheduler-related configuration is set in the cluster-level configuration (applied via `relish apply` or Lettuce) unless otherwise noted.

### Scheduler Core

| Knob | Default | Description |
|------|---------|-------------|
| `scheduler.tick_interval` | `100ms` | How often the scheduling queue is drained. Lower values reduce latency; higher values improve batching efficiency. |
| `scheduler.max_decisions_per_tick` | `500` | Maximum scheduling decisions per tick. Prevents a single tick from blocking too long. |
| `scheduler.pin_to_core` | `auto` | CPU core affinity for the scheduler task. `auto` pins to a dedicated core when the cluster exceeds 1,000 nodes. `none` disables pinning. An explicit core index can be specified. |
| `scheduler.learning_period_threshold` | `0.95` | Fraction of known nodes that must report before the learning period ends. |
| `scheduler.learning_period_timeout` | `15s` | Maximum duration of the learning period after leader election. |

### Placement

| Knob | Default | Description |
|------|---------|-------------|
| `scheduler.scoring.bin_packing_weight` | `50` | Weight for the bin-packing score (0-100). |
| `scheduler.scoring.preferred_label_weight` | `20` | Weight for preferred label matching (0-100). |
| `scheduler.scoring.image_locality_weight` | `15` | Weight for image cache locality (0-100). |
| `scheduler.scoring.stability_weight` | `5` | Weight for node uptime/stability (0-100). |

### Jobs

Job limits (active runs, definitions per namespace, retained receipts, chunk
size and per-node concurrency) are constants enforced by the council, and a
job's own policy lives in its `[job.NAME]` table; the
[manual](../manual/14_batch-jobs.md) lists both. There are no
`scheduler.batch.*` knobs, planned or otherwise: the allocation model they
configured was retired (§5.2).

### Rolling Deploys

| Knob | Default | Description |
|------|---------|-------------|
| `defaults.app.deploy.strategy` | `"rolling"` | Deploy strategy: `rolling` or `blue-green`. |
| `defaults.app.deploy.max_surge` | `1` | Default max extra instances during rolling deploy. |
| `defaults.app.deploy.drain_timeout` | `"30s"` | Default drain timeout for connection draining. |
| `defaults.app.deploy.health_timeout` | `"60s"` | Default timeout for health checks on new instances. |
| `defaults.app.deploy.max_unavailable` | `0` | Default max instances below target during a rolling deploy. |
| `defaults.app.deploy.auto_rollback` | `true` | Whether to roll back on health failure. |

### Autoscaling

| Knob | Default | Description |
|------|---------|-------------|
| `scheduler.autoscale.eval_interval` | `30s` | How often the autoscaler evaluates metrics. |
| `scheduler.autoscale.stabilization_window` | `5m` | Duration of the metric smoothing window. |
| `scheduler.autoscale.scale_up_cooldown` | `60s` | Minimum time between scale-up events for the same App. |
| `scheduler.autoscale.scale_down_cooldown` | `300s` | Minimum time between scale-down events for the same App. |
| `scheduler.autoscale.tolerance` | `0.1` | Metric must differ from target by at least this fraction to trigger scaling. Prevents flapping near the target. |

### Resource Reservations (per-node, in node.toml)

| Knob | Default | Description |
|------|---------|-------------|
| `resources.reserved_cpu` | `"500m"` | CPU reserved for system and Bun overhead on each node. |
| `resources.reserved_memory` | `"512Mi"` | Memory reserved for system and Bun overhead. |
| `resources.reserved_disk` | `"10Gi"` | Disk reserved for system use. |
| `network.port_range` | `{ start = 10000, end = 32767 }` | Host port range for container port allocation, below Linux's ephemeral range. |

---

## 7. Failure Modes

### 7.1 Node Failure During Job Execution

**Scenario:** A node holding chunks of a job run becomes unreachable mid-execution.

**Detection:** The leader polls each worker for its arrays' progress every tick. A holder that stays silent past the silence window, or answers that it can no longer run the array (its allowlist, runtime or `mount_isolation` changed), counts as lost.

**Response:**

1. Tasks the node already reported in an accepted chunk stay done: acceptance is durable, and each chunk result is fenced by its grant attempt, so a late answer from an old grant can't overwrite a newer one.
2. With `replay_unknown = true`, the leader requeues the node's chunks (`TaskArrayWrite::Requeue`) and grants them again at the next attempt, on whichever node fits.
3. Without it, the default, the leader records the node as an unknown owner of the run (`TaskArrayWrite::Unknown`). Its unfinished tasks aren't handed to anyone else, because they may have run. The run waits until an operator checks and writes `relish batch replay ID --node NODE --grant-digest DIGEST --acknowledge-side-effects`, which names the exact grant acknowledged. A cron definition with an unknown owner skips its occurrences meanwhile.

**Edge case:** A node that returns with work from before a new recovery epoch is refused: its array directories are kept behind a durable refusal, its attempts are cancelled, and an operator archives the old data and re-enrols it (see the appendix).

### 7.2 Scheduling with Incomplete State (Post-Election)

**Scenario:** A new leader is elected after a failure. Some nodes have not yet reported their state.

**Response:** The learning period mechanism prevents this from being a problem:

1. The new leader enters the learning period and collects `StateReport` messages from nodes.
2. It does not make scheduling decisions or accept new deploys during this period.
3. The learning period ends when 95% of nodes (by Mustard membership count) have reported, or after 15 seconds.
4. Unreported nodes are marked `NodeStatus::Unknown`. Meat will not schedule new work to them.
5. As `Unknown` nodes report in, they transition to `Ready` and become eligible.

**Worst case:** If the timeout fires with less than 95% reporting, the leader begins scheduling using only the capacity of reported nodes. This may temporarily reduce cluster capacity but avoids incorrect decisions. Latent nodes become available as they report.

### 7.3 Quota Exhaustion

**Scenario:** A namespace has consumed its entire CPU or memory quota. A new deploy or autoscale event requests additional resources.

**Response:**

- **New deploys:** `relish apply` succeeds and the app is committed, but Meat leaves it unplaced. The pass records why: the leader proposes a `RaftRequest::QuotaBlocked` carrying every blocked app and its `QuotaError`, which replaces `DesiredState::quota_blocked`. Every node serves that reason in its desired-app evidence (a node that isn't the leader fetches it from the leader) (`GET /v1/diagnostics/apps`, field `blocked`), so it shows wherever an operator looks:
  ```
  $ relish status
  new-service (namespace prod) is not placed, blocked: namespace "prod" would exceed CPU quota: 3000+2000 > 4000m
  ```
  `relish inspect` adds a `Blocked:` line, Brioche marks the app `blocked` and prints the reason on its page, and `relish wtf` raises a `quota-blocked` warning in place of the generic under-replication.
- **Clearing:** the reason belongs to the pass that found it. Raise the budget, or shrink or delete other apps in the namespace, and the next pass admits the app, places it and drops it from the set. Deleting the blocked app removes its reason in the same Raft entry.
- **No write churn:** an over-quota app stays blocked pass after pass, and the leader plans a pass every tick. It compares the pass's set with the recorded one (`quota_blocked_update`) and writes only when an app joins or leaves the set or its numbers change, so a steady over-quota cluster adds nothing to the Raft log. The `scheduler: quota rejects …` log line follows the same rule and appears only when the reason is new.
- **Autoscale and replica changes:** a scale-up that would bust the budget is skipped the same way, records the same reason, and the app keeps the placements it already has. Running workloads are never terminated to satisfy a quota.
- **Planned, not yet scheduled on the roadmap:** an apply-time error for an app that can't fit its namespace's budget, and a quota alert. The reason is a status, not an event: there's no history of when an app was blocked.

### 7.4 Leader Failure Mid-Deploy

**Scenario:** The leader fails while a rollout is in progress (some nodes on the new spec, others not).

**Response:** Nothing to resume, because there is no rollout state to lose. Each node's share rolls on its own deploy worker, which doesn't need the leader once it has the spec. The new leader rebuilds the release plan from the nodes' state reports (each reports the spec it last finished deploying) and the council's `AppRollouts` record, then carries on releasing the spec one node at a time. Until a node's report arrives, the new leader treats it as unsettled and may hold it; holding means "don't touch", so a stale view costs time, never a wrong deploy.

### 7.5 Losing One Node

**Scenario:** One node of three dies while an App runs one replica on each.

**Response:** Meat moves as little as it can.

1. Only a placement on a node gossip no longer lists as `Alive` or `Suspect` is lost, or one on a node that reported itself not ready or not capable, or whose reports have been stale for longer than the stale-node grace. A `Suspect` node missed a probe and has the suspicion timeout to refute it; in a three-node cluster that just lost a member there is no third node to relay an indirect probe, so a loaded survivor can be suspected for a late ack. Its placements hold until gossip declares it dead (#346). A live node whose readiness report hasn't reached a new leader yet keeps its placements too (§7.2).
   - **Stale but alive (D28).** Gossip can list a node alive while its report worker is wedged; the soak has seen workers stall for over a minute. Its replicas may well still be serving, and replacing them at once would duplicate them for a stalled reporter (the #423 lesson). So a stale node's placements hold for `[reporting_tree] stale_node_grace_secs` (default 300) from the moment its reports went stale, then move. The aggregator publishes when each node went stale (`AggregatedState::stale_since`): its last report plus `stale_report_timeout_secs`, or for a member the leader has never heard from, the moment this leadership epoch started listening, so a new leader gives every node the full grace. Daemon sets follow the same rule: a stale node's copy is held for the grace, then dropped. Until 0.2.1 fixed replicas on a stale node held for good while its daemon copies were dropped at once. A managed-volume app still never moves (§5.1).
2. Every placement that holds stays where it is. Meat places only the missing replicas.
3. Before placing them, the pass sets each node's replica count for the App from the kept placements (reports can lag a new leader), so spread sends each replacement to the eligible survivor running the fewest replicas. No node ends up with more than `ceil(replicas / eligible nodes)` unless the others lack room.
4. The survivor that gains a replica receives the same spec with a higher count. Its agent starts only the added replica beside the running ones; it rolls its replicas only when the spec changed in any other way, or when not every replica is running.
5. When the lost node returns, nothing moves back: the App is converged, and rebalancing would restart healthy replicas.

### 7.6 Cascading Node Failures

**Scenario:** Multiple nodes fail simultaneously (rack failure, network partition).

**Response:**

1. For each failed node's App instances, Meat attempts to reschedule to surviving nodes.
2. If surviving capacity is insufficient, Meat places the replicas that fit and records each short app's reason (§5.1, Short Capacity): `relish status` and `relish wtf` show it, and the leader records a `placement` event. No Mayo alert fires yet.
3. Rescheduling currently processes apps in a deterministic order (alphabetical by app id), not by remaining healthy replicas. Priority ordering by criticality — rescheduling the apps with the fewest remaining healthy replicas first — is **planned, not yet implemented**.
4. Job chunks on failed nodes are requeued or held as unknown, as described in 7.1.

---

## 8. Security Considerations

### 8.1 Scheduler Spoofing

**Threat:** A compromised node sends forged scheduling decisions, pretending to be the leader.

**Mitigation:**

- All communication between Meat and Bun agents flows through the mTLS channel managed by Sesame. Scheduling decisions are signed by the leader's node certificate.
- Bun agents verify that scheduling directives come from the current Raft leader (whose identity they know via Mustard gossip).
- Scheduling decisions are committed to the Raft log before being executed. A compromised non-leader node cannot write to the Raft log (it is not a council member, or if it is, it is not the leader).
- The Raft log itself provides an audit trail: every scheduling decision is attributed to the leader that made it.

### 8.2 Resource Exhaustion Attacks

**Threat:** A malicious user submits workloads that exhaust cluster resources, starving other tenants.

**Mitigation:**

- **Namespace quotas** cap the total CPU, memory, GPU, app count, and replica count per namespace. A single tenant cannot exceed their quota.
- **Admission validation:** Meat validates all resource requests at submission time. Requests exceeding remaining quota are rejected immediately.
- **Job admission caps:** the council bounds active runs (128 in the cluster, 32 of them kept for cron fires and deployment hooks, 64 per namespace), job definitions and retained receipts, and a run's `per_node_concurrency` caps how many of its tasks one node runs at once. Each attempt also reserves its CPU and memory in the node's execution budget.
- **Rate limiting (planned):** per-identity rate limits on deploy and job submission don't exist yet; the admission caps above are what bound a flood today.

### 8.3 Process Workload Escalation

**Threat:** A process workload (exec/script job) attempts to escape its sandbox.

**Mitigation:**

- Process workloads require explicit `admin` or `host-exec` permission.
- Operators must configure a binary allowlist in `node.toml`. Meat's Filter phase rejects process workloads targeting nodes without the required binary in the allowlist.
- Even after scheduling, Bun enforces cgroup isolation, PID namespace, restricted mount namespace, seccomp profile, and the dedicated `burger` user.

### 8.4 Scheduling Decision Replay

**Threat:** An attacker replays an old scheduling decision to start a workload that should no longer be running.

**Mitigation:**

- **Planned — not yet implemented:** giving each `SchedulingDecision` a monotonically increasing `decision_id` and a `spec_version`, so Bun agents can reject decisions with a `decision_id` lower than or equal to the last one they processed for a given App. The shipped `SchedulingDecision` carries neither field yet, so this per-decision replay check does not exist today.
- Decisions are committed to the Raft log, which has its own monotonic term and index. Replay of old Raft entries is prevented by the Raft protocol itself — this is the protection that is actually in force.

---

## 9. Performance

### 9.1 Throughput Targets

The target is the whitepaper's: about 1,150 accepted jobs a second sustained
(100M a day). An earlier version of this section showed a table of "measured"
rates and latencies from a 12-node bench that was never run; we deleted it.
What exists instead:

- `benches/task_arrays.rs` measures, in one process, the leader's control-plane
  cost for a 1M-task array on 3 and 200 nodes, one leader tick with 64
  concurrent arrays, the node pool's overhead per task with a fake runner, the
  machine's fork/exec floor, and a worker's disk cost per task.
- The job qualification runs measure real clusters at the #668 target (1,390
  accepted a second by default) with node-level faults. The 0.2.0 records are
  [`2026-10-17-v0.2.0-job-qualification.md`](../qualification/2026-10-17-v0.2.0-job-qualification.md)
  and the [throughput record](../qualification/2026-10-17-v0.2.0-job-throughput/);
  [testing.md](../testing.md) describes the tiers.

### 9.2 Latency Bounds

These are design targets, not measurements; §9.4 has what `benches/scheduler.rs` measured.

- **App placement decision:** < 5ms for a single replica on a 10,000-node cluster. The Filter phase is O(N) in nodes; an index of nodes by label combination would cut that and is planned. The Score phase runs only on filtered candidates.
- **Job chunk grants:** one leader tick plans every array's grants and commits them in one `Tick` entry; `benches/task_arrays.rs` measures a tick with 64 concurrent arrays.
- **Rolling deploy step:** Dominated by health_timeout (default 60s), not scheduler overhead. The scheduling overhead per step is < 10ms.
- **Autoscale evaluation:** < 5ms per App. With 1,000 autoscaled Apps, the full evaluation cycle completes in < 5 seconds (well within the 30-second default interval).

### 9.3 Memory Budget at 10,000 Nodes

```
NodeCapacity per node:   ~2 KB (labels, resource counters, image set)
10,000 nodes:            ~20 MB

Per-App scheduling state: ~500 bytes
5,000 Apps (500/node avg): ~2.5 MB

Job state:               compact index ranges per active run,
                         bounded by the active-run caps (§8.2)

Rollout record:          up to ten specs per app (AppRollouts)

Autoscale state:         ~4 KB per App (metric sample window)
1,000 autoscaled Apps:   ~4 MB

Total Meat memory:      ~30 MB estimated (at 10,000 nodes, peak; unmeasured)
```

This fits comfortably within a typical server's memory. The 312 MB total Bun agent memory (at 500 apps) noted in the whitepaper benchmarks includes all Bun subsystems, not just Meat.

### 9.4 CPU Budget

Meat's scheduling loop is designed to consume < 15% of a single core at sustained throughput. At the 1,150 jobs/sec target:

- App placement is measured by `benches/scheduler.rs` (`make bench`). One leader pass placing 150 replicas (50 apps of 3) took about 29 ms on 1,000 nodes, about 190 µs per placement, and about 330 ms on 10,000 nodes, about 2.2 ms per placement, on the development laptop (an Apple silicon MacBook, October 2026). Filter and Score each walk every node for every replica, so the cost grows with the cluster. A converged pass, the one the leader runs every tick once everything is placed, took about 0.2 ms on 1,000 nodes and 1.7 ms on 10,000. The Raft commit comes on top, once per pass rather than per decision.
- An earlier version of this section estimated ~270 µs per decision with nothing to measure it against. The measurement above is close at 1,000 nodes and an order of magnitude off at 10,000; App placement rates are far below the job rates below, so it fits the budget, but per-node work in Filter and Score is the place to look if large clusters need faster placement.
- With core pinning at > 1,000 nodes, this would be isolated from other Bun tasks (core pinning is planned, not yet implemented — see Section 1).

Jobs don't go through placement at all: the leader grants chunks of task indices, so its work grows with chunks and ticks, not with tasks. `benches/task_arrays.rs` measures that cost (§9.1).

---

## 10. Testing Strategy

### 10.1 Unit and Property Tests

What ships are property tests over random clusters (`meat::scheduler::properties`, with proptest): placement never overcommits a node, spreads no more than `ceil(replicas / eligible nodes)` per node when every node has room, gives the same plan whatever order the nodes were inserted in, and a pass never places more CPU than a namespace's quota. The unit tests in `cluster::orchestrate` drive `plan_pass` directly, including the stale-node grace and short capacity (`scheduler_policy`), and a proptest there checks that a capacity refusal moves exactly the replicas it names (`a_refusal_moves_exactly_the_refused_replicas`). The rollout planner's tests in `meat::deploy_types` walk whole rollouts through `plan_rolling_step`, and `cluster::rollout` tests the one-node-at-a-time release. Jobs are tested in `meat::task_array_state`, `meat::task_array_store` and `bun::task_array_leader`, from chunk planning to unknown owners and replay.

A deterministic whole-scheduler simulation, with fake Raft, gossip and reporting driving a `SchedulerSimulation` through deploys, node failures and simulated time, was the original design. It's **planned, not yet implemented**; the property tests above cover its placement scenarios, and the cluster tests below cover the rest against real nodes.

### 10.2 Integration Tests

The built-in `relish test` suite (`src/testkit/cases/`) exercises Meat against a live cluster:

| Case | What it checks |
|------|----------------|
| `schedule_fixed_replicas_across_nodes` | Basic placement spread |
| `schedule_respects_required_placement_label` | Hard constraint enforcement |
| `schedule_rejects_app_exceeding_namespace_quota` | Quota enforcement |
| `rolling_deploy_keeps_the_app_running` | Continuous health probe during a rollout |
| `failed_deploy_rolls_back_automatically` | Rollback of a broken version |
| `deploy_history_records_each_version` | Deploy history |
| `job_runs_to_completion_and_reports_exit` | A job's run and exit code |
| `scheduled_job_fires_on_its_schedule` | Cron |
| `job_logs_are_retrievable_after_completion` | Retained job output |

There are no daemon-set or preferred-label cases yet. The repository's own cluster suites (`tests/placement.rs` and `tests/cluster_task_arrays.rs`, run by `make test-cluster`) cover three real nodes: spread, rollouts one node at a time, `RolloutFailed`, rollback after restarts, and job arrays across nodes.

### 10.3 Chaos Tests

From the `relish test --chaos` suite (`src/testkit/chaos/scenarios.rs`):

| Case | What it checks |
|------|----------------|
| `leader_failure_elects_new_leader_and_cluster_recovers` | A new leader takes over and the cluster recovers |
| `dead_worker_node_has_workloads_rescheduled` | Replicas move to surviving nodes |
| `minority_partition_degrades_and_heals` | A partitioned minority degrades and rejoins |
| `resource_exhaustion_degrades_gracefully` | Pressure on a node doesn't take the cluster down |
| `node_death_during_deploy_ends_clean` | A node lost mid-rollout leaves no half-finished state |

Draining a node with zero-downtime migration needs drain itself, which is #656.

### 10.4 Benchmark Tests

- `benches/scheduler.rs` (`make bench`): one leader pass placing replicas on 1,000 and 10,000 nodes, and a converged pass (§9.4).
- `benches/task_arrays.rs`: the job control plane in one process (§9.1).
- `relish bench --scenario …` runs timed job scenarios against a real cluster (60 seconds at concurrency 27 by default) and can save and compare JSON reports; the job qualification scripts build on it.

---

## 11. Prior Art

### Kubernetes Default Scheduler

The Kubernetes scheduler uses a similar filter-then-score pipeline (called "predicates" and "priorities" in older versions, now "filter" and "score" plugins in the Scheduling Framework). It evaluates all nodes for each Pod and selects the highest-scoring node.

**What we borrow:** The filter/score two-phase architecture is well-proven and extensible. Meat adopts this structure.

**What we do differently:**

- Kubernetes scores with a plugin framework that supports arbitrary user-defined scorers. Meat uses a fixed set of scoring dimensions with configurable weights, trading extensibility for simplicity and predictable performance.
- Kubernetes schedules every Pod individually through the scheduler, including batch Jobs. This becomes a bottleneck at high Job throughput. Meat delegates batch execution to nodes.
- Kubernetes has no built-in concept of namespace resource quotas enforced at scheduling time (ResourceQuotas exist but are enforced by the API server admission controller, not the scheduler). Meat enforces quotas directly in the scheduler to prevent scheduling decisions that would violate quotas.

**Reference:** [Kubernetes Scheduling Framework (KEP-624)](https://github.com/kubernetes/enhancements/tree/master/keps/sig-scheduling/624-scheduling-framework), [Kubernetes Scheduler source](https://github.com/kubernetes/kubernetes/tree/master/pkg/scheduler)

### HashiCorp Nomad

Nomad uses a bin-packing scheduler for services and a batch scheduler for batch jobs. The bin-packing algorithm maximizes node density, and Nomad supports both `binpack` and `spread` strategies as first-class options.

**What we borrow:** Nomad's bin-packing approach and its clear separation between service (long-running) and batch (run-to-completion) scheduling. Meat's bin-packing score computation is directly inspired by Nomad's.

**What we do differently:**

- Nomad evaluates scheduling plans using a plan-apply model with optimistic concurrency. Multiple schedulers can compute plans concurrently, and the leader merges them. This is more complex than Meat's single-leader model but provides higher throughput in multi-datacenter deployments. Meat's single-leader model is simpler and sufficient for the single-cluster use case.
- Nomad's batch scheduler queues individual allocations. Meat grants chunks of task indices to nodes, keeping the leader's hot path proportional to chunks rather than jobs.

**Reference:** [Nomad Scheduler Design](https://developer.hashicorp.com/nomad/docs/concepts/scheduling/scheduling), [Nomad Architecture](https://developer.hashicorp.com/nomad/docs/concepts/architecture)

### Google Borg

Borg is Google's internal cluster manager. Its scheduler handles both long-running "production" workloads and batch workloads in a single system. Borg uses a priority-based model where production workloads preempt batch workloads when resources are scarce.

**What we borrow:** Borg's batch model -- the insight that high-throughput batch scheduling requires delegation rather than centralized per-task decisions. Borg assigns "allocs" (resource reservations) and lets tasks fill them. Meat's chunk grants, where a worker takes a range of task indices and fills it within its own execution budget, are a simplified version of this concept.

**What we do differently:**

- Borg supports preemption (killing low-priority batch jobs to make room for production workloads). Meat v1 does not implement preemption -- all workloads are co-equal once scheduled. Preemption is a v2 consideration.
- Borg uses "cells" of ~10,000 machines each, managed by a single Borgmaster. Meat targets the same scale (10,000 nodes) with a conceptually similar single-leader architecture.

**Reference:** [Large-Scale Cluster Management at Google with Borg (EuroSys 2015)](https://research.google/pubs/pub43438/)

### Google Omega

Omega is a research system exploring shared-state, lock-free, optimistic concurrent scheduling. Multiple schedulers share a full view of the cluster state and make scheduling decisions in parallel, resolving conflicts via optimistic concurrency control.

**What we borrow:** Omega's analysis of the scheduling bottleneck at scale informed the decision to delegate batch execution rather than schedule individually. However, Meat does not adopt Omega's shared-state model.

**What we do differently:** Omega's optimistic concurrency adds complexity (conflict resolution, retries, stale-state handling). Meat's single-leader model avoids these entirely. The tradeoff is that Meat cannot scale scheduling throughput horizontally by adding schedulers. The delegated batch model compensates by reducing the decision rate.

**Reference:** [Omega: Flexible, Scalable Schedulers for Large Compute Clusters (EuroSys 2013)](https://research.google/pubs/pub41684/)

### Summary

| Aspect | Kubernetes | Nomad | Borg | Omega | Meat |
|--------|-----------|-------|------|-------|-------|
| Architecture | Single scheduler | Multi-scheduler (optimistic) | Single Borgmaster | Shared-state | Single leader |
| Batch model | Per-Pod | Per-allocation | Delegated allocs | Per-task (concurrent) | Delegated batches |
| Bin-packing | Plugin (LeastAllocated/MostAllocated) | First-class (binpack/spread) | Built-in | Per-scheduler policy | Fixed with configurable weights |
| Quota enforcement | API server admission | Namespace quotas | Quota via priority | N/A | Scheduler admission |
| Complexity | High (plugin framework) | Medium (plan-apply) | High (preemption, cells) | High (OCC) | Low (single-leader, fixed scoring) |

---

## 12. Libraries & Dependencies

Meat is compiled into the Reliaburger binary. The following Rust crates are used within the scheduler:

| Crate | Version | Purpose |
|-------|---------|---------|
| `tokio` | 1.x | Async runtime for the scheduling loop, timers, and channel communication. |
| `tokio::sync::mpsc` | (part of tokio) | Unbounded channels for the scheduling queue (deploy requests flow in, decisions flow out). |
| `serde` + `serde_json` | 1.x | Serialisation for Raft log entries (`SchedulingDecision`, `TaskArrayWrite`), which must be self-describing; JSON for API responses. |

No external scheduling frameworks or orchestration libraries are used. The scheduling algorithm is implemented directly in Reliaburger-specific code to avoid abstraction overhead and maintain full control over the hot path.

---

## 13. Open Questions

### 13.1 Preemption

Should Meat support preempting lower-priority batch jobs to make room for App placements when the cluster is at capacity? Borg does this. The argument for: it prevents Apps from being unschedulable when batch jobs have claimed all resources. The argument against: it adds complexity and makes batch job completion times unpredictable. **Current decision: deferred to v2.**

### 13.2 Multi-Scheduler for Batch

At extreme scale (10,000+ nodes, sustained burst rates above 5,000 jobs/sec), should Meat delegate batch scheduling to secondary schedulers on non-leader nodes? This would adopt an Omega-like model for batch only while keeping App scheduling on the leader. **Current decision: not needed -- the delegated batch model keeps leader decision rate at O(nodes), not O(jobs). Revisit if benchmarks show a bottleneck.**

### 13.3 Fractional GPUs

V1 supports whole-device GPU allocation only (`gpu = 1`, `gpu = 2`). Fractional GPU sharing via NVIDIA MIG (Multi-Instance GPU) or time-slicing would enable better utilisation for inference workloads that do not need a full GPU. **Current decision: deferred to v2. MIG partitions would be exposed as separate schedulable devices (e.g., `gpu_mig_3g.20gb = 1`).**

### 13.4 Spread Strategy as First-Class Alternative

Currently, bin-packing is the primary strategy and spread is a scoring component. Some workloads (latency-sensitive services) benefit from a spread-first strategy that distributes replicas across as many nodes as possible, even if this reduces density. Should Meat support a per-App `strategy = "spread"` that inverts the scoring weights? **Current decision: under consideration. Spread is already the first sort key, so replicas land on the nodes running fewest of them; a dedicated spread mode would go further and prefer emptier nodes over fuller ones as well.**

### 13.5 Topology-Aware Scheduling

Should Meat understand rack topology (from node labels like `rack = "rack-3"`) and enforce spread across racks, not just across nodes? This would protect against rack-level failures. **Current decision: under consideration. Rack-aware spread could be implemented as an additional scoring dimension or as a hard constraint (e.g., `placement.spread_by = "rack"`).**

### 13.6 Autoscaler Custom Metrics

The autoscaler supports exactly `cpu` and `memory`; config validation rejects any other `metric`. An earlier draft of this doc listed a `Custom(String)` metric path. Nothing implemented it, and the loop quietly never scaled on it, so 0.1.0 drops the promise. **Open question for later: should custom metrics be PromQL expressions evaluated by Mayo, or simple metric-name + aggregation-function pairs?** Either way they'd need a stated unit (utilisation against what?) before they could share the `target` field with `cpu` and `memory`.

### 13.7 Scheduling Latency SLO Enforcement

Should Meat provide a hard guarantee that scheduling decisions complete within a latency bound (e.g., 50ms)? Currently, latency is best-effort and depends on cluster size and Raft commit speed. An SLO enforcement mode could shed load (reject submissions) when latency exceeds the bound. **Current decision: under consideration for v2.**

## Delegated task-array implementation (0.2.0)

PR #266 implements compact homogeneous arrays and atomic manifests of up to
sixteen profiles. The council grants chunks and accepts aggregate terminal
outcomes. It does not place or replicate a `JobInstance` per index. Eligible
workers report how many requests fit; a node-wide `ExecutionBudget` reserves
CPU and memory for apps and actual batch attempts. Chunk size is queue depth,
not simultaneous resource use. App creation, rolling replacements and recovery
must use the same budget; stopped owners retain requests until retirement.

Batch requests wait FIFO without partial CPU or memory reservations. This
prevents starvation by repeated small requests, with a head-of-line utilisation
trade-off. It is not tenant DRF, and running jobs do not automatically yield to
a new app deployment. Provision service/rollout headroom. A profile larger than
a node's allocatable capacity is refused; temporary fullness queues work.

App placement sees what jobs hold. Each `StateReport` carries the node's
`job_reservations` (the budget's task-attempt and executor leases, quarantined
ones included) apart from `resource_usage`, which sums app requests per
instance. `build_cluster_cache` adds both into the node's `allocated` total,
so `reserve_commitments` only reconciles committed app placements and never
counts an app twice. A placement can still lose the race to attempts that start
after the pass: `try_acquire` doesn't queue, so the agent records a
`CapacityRefusal` for that app and the next report carries it. The refusal
names the replica ordinals the node couldn't start (#971): a scale-up names
only the replicas it tried to add, while a fresh deploy and a rolling or
blue-green replacement name the node's whole share, since none of the new
spec's replicas can run there. The leader stops treating those placements as
held (`Liveness::keeps`) and places them elsewhere; the app's other replicas
on that node stay where they run, and a managed-volume app still waits for its
home node. With nowhere to go, a refused replica stays committed where it was,
and the shortfall is logged once, when it starts or changes (§5.1).

A refusal lasts until an admission of that app succeeds on the node, or the
app is stopped or retired there (#967). A refused scale-up records the spec the
node ran before again, since it started nothing, so the node's retry adds the
same replicas in place instead of rolling the app, which would need room for
every new replica beside the old one. When the leader moves the refused
replicas away, the node's share is the spec it already runs: that deploy adds
nothing, rolls nothing, and its successful admission clears the refusal.

Workers persist control provenance (recovery epoch, leadership term, committed
log index) and highest chunk generation. Task outcomes stream into checksummed
ledgers and derived indexes; chunk acceptance follows durable acknowledgements.
Accepted owner/generation ranges select detail and failure output. Disk failure
stops local admission; uncertain runtime retirement retains the slot and request.
Execution remains at least once, so external effects require business idempotency
keys. A new recovery epoch refuses workers with existing array directories,
cancels their attempts and preserves old outcomes behind a durable refusal.
Archive old worker data and re-enrol with fresh data after disaster recovery;
this avoids reusing grant generations from a rolled-back council snapshot.

Rootful Linux images use the existing owned runtime, cached layers, reusable
instance slots and fresh attempt generations. Reuse bounds retained metadata per
namespace; it does not eliminate per-attempt runc launch cost. With eBPF enabled,
image executors inherit a journalled namespace binding before launch, without
rewriting the app firewall journal for every task. Exact app bindings take
precedence. Cross-namespace batch service access is refused; binding loss retires
the original owner, and startup cleans cached bindings only after old executors
retire. Summary APIs,
watch and the dashboard avoid task enumeration; indexed pages bound inspected
indexes and returned rows and contact at most eight workers per page. Terminal
retention begins at completion. See the
[manual](../manual/14_batch-jobs.md) and [implementation plan](../plans/archive/2026-10-04-plan-delegated-jobs.md)
for limits and qualification requirements.
