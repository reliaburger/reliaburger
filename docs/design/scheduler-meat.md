# Meat: The Reliaburger Scheduler

## 1. Overview

Meat is the centralized scheduling engine of the Reliaburger container orchestrator. It runs exclusively on the current Raft leader node as a dedicated async task and is responsible for all workload placement decisions in the cluster: placing App replicas across nodes, allocating batch Jobs at high throughput, orchestrating rolling deployments, enforcing namespace resource quotas, and driving autoscaling reactions.

### Role in the System

Meat is the single decision-maker for "what runs where." Every App placement, Job allocation, rolling deploy step, and autoscale event flows through Meat. It consumes desired state (from the Raft log or Lettuce GitOps engine), combines it with the cluster's reported runtime state (from the hierarchical reporting tree), and emits scheduling decisions that are committed to Raft and distributed to Bun agents on target nodes.

### Key Design Decisions

- **Single-leader scheduling.** All scheduling decisions are made on one node. This eliminates coordination overhead between multiple schedulers and makes the Raft log the single source of truth for placement decisions. The tradeoff -- a single point of decision-making -- is mitigated by fast leader failover (< 5 seconds) and the delegated batch model.

- **Delegated batch execution.** Meat does not schedule individual Jobs from high-throughput batch workloads. Instead, it allocates capacity budgets to nodes ("Node 7, here are your next 200 jobs"), and nodes execute and report completions asynchronously. This is the mechanism that enables 100M+ jobs/day without the leader becoming a bottleneck.

- **Bin-packing first.** Meat uses a bin-packing algorithm as its primary placement strategy, maximising node utilisation before spreading to new nodes. This reduces the number of active nodes under low load and improves cache locality for images already present on a node.

- **Dedicated CPU budget.** When the cluster exceeds 1,000 nodes, Meat's async task is pinned to a specific CPU core on the leader node, ensuring that API serving, Brioche UI, metrics queries, and other leader responsibilities cannot starve the scheduling loop.

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
| **Mayo (Metrics)** | Soft | Meat reads Mayo metrics for autoscaling decisions (CPU utilisation, custom metrics). Mayo runs on every node; the leader queries aggregated metrics from council members. |
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
│  │                   Batch Allocator                           │ │
│  │                                                             │ │
│  │  - Partitions pending jobs across eligible nodes            │ │
│  │  - Sends batch assignments via reporting tree               │ │
│  │  - Tracks completion counts, retries, timeouts              │ │
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
- For process workloads: node has the required binary in its allowlist.

**Phase 2: Score.** Rank remaining candidate nodes (0-100 scale, higher is better).

- **Bin-packing score (weight 50):** Prefer nodes with the least remaining allocatable resources after placing this workload. This maximizes density.
- **Preferred label score (weight 20):** Nodes matching `preferred` labels receive a bonus.
- **Image locality score (weight 15):** Nodes that already have the required image layers cached in Pickle score higher.
- **Spread score (weight 10):** Penalize nodes that already run other replicas of the same App. This provides failure-domain diversity.
- **Node stability score (weight 5):** Prefer nodes with longer uptime and no recent restarts.

**Phase 3: Select.** Pick the highest-scoring node. Ties are broken by node ID (deterministic). For multi-replica placements, Meat runs the pipeline iteratively, updating the cluster state cache after each placement to reflect the newly committed resources.

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

All types are Rust structs using `serde` for serialisation (Raft log entries are serialised with `bincode` for compactness; API responses use JSON).

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
    /// the workload remains unscheduled and an alert fires.
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

/// Deploy strategy configuration for an App.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployStrategy {
    /// Deployment strategy type. Currently only "rolling" is supported.
    pub strategy: DeployStrategyType,
    /// Maximum number of extra instances during a rolling update.
    pub max_surge: u32,
    /// Time to wait for in-flight requests to drain before stopping old instances.
    pub drain_timeout: Duration,
    /// Time to wait for health checks to pass on new instances.
    pub health_timeout: Duration,
    /// If true, revert all instances to the previous version on health check failure.
    /// If false (default), halt the rollout and leave partial state.
    pub auto_rollback: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DeployStrategyType {
    Rolling,
}

impl Default for DeployStrategy {
    fn default() -> Self {
        Self {
            strategy: DeployStrategyType::Rolling,
            max_surge: 1,
            drain_timeout: Duration::from_secs(30),
            health_timeout: Duration::from_secs(60),
            auto_rollback: false,
        }
    }
}

/// Autoscaling configuration for an App.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoscaleConfig {
    /// Metric to scale on: "cpu", "memory", or a custom metric path.
    pub metric: AutoscaleMetric,
    /// Target value for the metric (e.g., 70 for 70% CPU utilisation).
    pub target: u32,
    /// Minimum replica count. Autoscaler will never scale below this.
    pub min: u32,
    /// Maximum replica count. Autoscaler will never scale above this.
    pub max: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AutoscaleMetric {
    Cpu,
    Memory,
    Custom(String),
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
    pub deploy: DeployStrategy,
    pub autoscale: Option<AutoscaleConfig>,
    pub init_containers: Vec<InitContainer>,
    pub env: BTreeMap<String, String>,
    pub gpu: u32,
    /// Dependency ordering: jobs that must complete before this app starts.
    pub run_after_jobs: Vec<JobId>,
    /// Spec version, incremented on each change. Used to detect stale state.
    pub spec_version: u64,
}

/// Full specification for a Job as consumed by the scheduler.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobSpec {
    pub id: JobId,
    pub source: WorkloadSource,
    pub command: Vec<String>,
    /// Cron schedule expression. If None, the job is on-demand only.
    pub schedule: Option<String>,
    /// Maximum concurrent instances for batch jobs.
    pub parallelism: u32,
    /// Number of retries on failure per instance.
    pub retry: u32,
    /// Maximum duration per instance before it is killed.
    pub timeout: Duration,
    pub resources: Resources,
    pub placement: PlacementConstraint,
    pub gpu: u32,
    /// Apps that depend on this job completing (run_before relationship).
    pub run_before: Vec<AppId>,
    /// Whether this is a build job with Pickle write access.
    pub build: bool,
    /// Scoped Pickle repositories this build job can push to.
    pub build_push_to: Vec<String>,
    /// Replica mode for scheduled jobs: fixed count or daemon ("*").
    pub replicas: ReplicaMode,
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
    /// Number of GPU devices available (detected via NVML).
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

```rust
/// A placement decision for a single App replica.
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

/// A batch allocation decision for high-throughput jobs.
/// Instead of scheduling individual jobs, Meat sends a batch to a node.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchAllocation {
    pub allocation_id: u64,
    pub job_id: JobId,
    pub target_node: NodeId,
    /// Number of job instances assigned to this node in this batch.
    pub instance_count: u32,
    /// Resources budgeted for this batch on the target node.
    pub resource_budget: Resources,
    /// Per-instance timeout.
    pub instance_timeout: Duration,
    /// Per-instance retry limit.
    pub retry_limit: u32,
    pub timestamp: Instant,
}

/// Report from a node about batch job completion.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchCompletionReport {
    pub allocation_id: u64,
    pub job_id: JobId,
    pub node_id: NodeId,
    pub completed: u32,
    pub failed: u32,
    pub retries_exhausted: u32,
    /// Instances still running at report time.
    pub in_progress: u32,
}
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

```rust
/// State machine for an in-progress rolling deploy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RollingDeployState {
    pub app_id: AppId,
    pub deploy_id: u64,
    pub old_image: String,
    pub new_image: String,
    pub strategy: DeployStrategy,
    /// Instances already upgraded to the new version.
    pub upgraded: Vec<InstanceState>,
    /// Instances still running the old version.
    pub pending: Vec<InstanceState>,
    /// Current step in the rolling process.
    pub phase: RollingDeployPhase,
    pub started_at: Instant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstanceState {
    pub instance_id: String,
    pub node_id: NodeId,
    pub image: String,
    pub port: u16,
    pub health: HealthStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HealthStatus {
    Starting,
    Healthy,
    Unhealthy,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RollingDeployPhase {
    /// Starting a new instance on the next target node.
    StartingNew { target_node: NodeId },
    /// Waiting for the new instance's health check to pass.
    WaitingHealth { instance_id: String, deadline: Instant },
    /// Draining the old instance (Wrapper removes from routing pool).
    Draining { old_instance: String, deadline: Instant },
    /// Stopping the old instance and releasing its port.
    StoppingOld { old_instance: String },
    /// Deploy completed successfully.
    Completed,
    /// Deploy halted due to health check failure (without auto_rollback).
    Halted { reason: String },
    /// Rolling back all upgraded instances to the old version.
    RollingBack,
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

1. Meat validates the spec (schema, permissions, namespace quota).
2. For each replica (0..3), Meat runs the four-phase placement pipeline:
   - **Filter:** Eliminate nodes that lack resources, do not match `required` labels, or are not ready.
   - **Score:** Rank candidates using the weighted scoring model (bin-packing 50%, preferred labels 20%, image locality 15%, spread 10%, stability 5%).
   - **Select:** Pick the highest-scoring node.
   - **Commit:** Reserve resources on the selected node in the cluster state cache, then commit the `SchedulingDecision` to the Raft log.
3. After all replicas are committed, the decisions are disseminated to target Bun agents.

The iterative per-replica approach (rather than computing all placements at once) ensures that the spread penalty accumulates correctly -- the second replica of an App scores a given node lower if the first replica was already placed there.

#### Required Labels (Hard Constraints)

```toml
[app.inference.placement]
required = { zone = "us-east-1", gpu_model = "a100" }
```

During the Filter phase, every node's `labels` map is checked. A node passes the filter only if for every key-value pair in `required`, the node has that exact label. This is a logical AND across all required labels. If zero nodes pass the filter, the App remains unscheduled, an alert fires via Mayo, and the event is logged.

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
fn bin_packing_score(node: &NodeCapacity, request: &Resources) -> f64 {
    let remaining_cpu = node.allocatable.cpu_millis - node.allocated.cpu_millis - request.cpu_millis;
    let remaining_mem = node.allocatable.memory_bytes - node.allocated.memory_bytes - request.memory_bytes;

    let cpu_utilization = 1.0 - (remaining_cpu as f64 / node.allocatable.cpu_millis as f64);
    let mem_utilization = 1.0 - (remaining_mem as f64 / node.allocatable.memory_bytes as f64);

    // Weighted average: we want both dimensions to be full, penalize imbalance.
    let avg = (cpu_utilization + mem_utilization) / 2.0;
    let imbalance_penalty = (cpu_utilization - mem_utilization).abs() * 0.1;

    ((avg - imbalance_penalty) * 50.0).clamp(0.0, 50.0)
}
```

Higher utilisation after placement is better (the node is more "full"). An imbalance penalty discourages placing CPU-heavy workloads on memory-heavy nodes and vice versa, which would leave stranded resources.

#### Daemon Mode (`replicas = "*"`)

When `replicas = "*"` is specified, Meat does not run the placement pipeline. Instead:

1. It queries all nodes from the cluster state cache.
2. If placement `required` labels are specified, it filters to matching nodes.
3. One instance is scheduled on every qualifying node.
4. Meat subscribes to Mustard membership events: when a new node joins the cluster and matches the placement constraints, an instance is automatically scheduled. When a node leaves, the instance is removed.

Daemon mode is not bin-packed -- every qualifying node gets exactly one instance regardless of its current load. This is appropriate for system-level workloads (node exporters, log forwarders, caches).

### 5.2 Batch Job Allocation (Delegated Model)

The 100M jobs/day target (approximately 1,150 jobs/sec sustained) requires a fundamentally different approach than scheduling each job individually through Raft.

#### Allocation Flow

1. A batch job spec arrives (e.g., `job.render-frame` with `parallelism = 1000`).
2. Meat computes eligible nodes using the Filter phase (resources, labels, process workload allowlist).
3. Meat partitions the total instance count across eligible nodes, weighted by available resources:

```rust
fn allocate_batch(
    job: &JobSpec,
    total_instances: u32,
    eligible_nodes: &[NodeCapacity],
) -> Vec<BatchAllocation> {
    // Compute each node's share based on available resources.
    let total_capacity: f64 = eligible_nodes.iter()
        .map(|n| compute_job_capacity(n, &job.resources))
        .sum();

    let mut allocations = Vec::new();
    let mut remaining = total_instances;

    for node in eligible_nodes {
        if remaining == 0 { break; }
        let node_capacity = compute_job_capacity(node, &job.resources);
        let share = ((node_capacity / total_capacity) * total_instances as f64).ceil() as u32;
        let count = share.min(remaining);
        remaining -= count;

        allocations.push(BatchAllocation {
            allocation_id: next_allocation_id(),
            job_id: job.id.clone(),
            target_node: node.node_id.clone(),
            instance_count: count,
            resource_budget: Resources {
                cpu_millis: job.resources.cpu_millis * count,
                memory_bytes: job.resources.memory_bytes * count as u64,
                gpus: job.resources.gpus * count,
                disk_bytes: 0,
            },
            instance_timeout: job.timeout,
            retry_limit: job.retry,
            timestamp: Instant::now(),
        });
    }
    allocations
}

/// How many instances of this job a node can run concurrently.
fn compute_job_capacity(node: &NodeCapacity, per_instance: &Resources) -> f64 {
    let cpu_slots = (node.allocatable.cpu_millis - node.allocated.cpu_millis) / per_instance.cpu_millis;
    let mem_slots = (node.allocatable.memory_bytes - node.allocated.memory_bytes) / per_instance.memory_bytes;
    let gpu_slots = if per_instance.gpus > 0 {
        (node.gpu_count - node.allocated.gpus) / per_instance.gpus
    } else {
        u32::MAX
    };
    cpu_slots.min(mem_slots).min(gpu_slots) as f64
}
```

4. Meat commits a single `BatchAllocation` entry per node to the Raft log (not per-instance).
5. Bun agents on target nodes receive their allocation and execute instances locally, managing concurrency up to the resource budget.
6. Nodes report `BatchCompletionReport` back via the hierarchical reporting tree at configurable intervals (default: every 5 seconds or on batch completion, whichever is sooner).
7. If a node reports `retries_exhausted > 0`, Meat re-allocates those failed instances to other eligible nodes.

This model means the Raft log records O(nodes) entries per batch, not O(instances). For a 1000-instance batch across 50 nodes, that is 50 Raft entries instead of 1000.

#### Scheduled Jobs (Cron)

Meat maintains a cron scheduler (using a priority queue keyed by next-fire-time). When a scheduled job's cron expression matches, Meat treats it as a new job submission:

- If `parallelism` is set, it goes through the batch allocation path.
- If `replicas = "*"`, the job runs on every qualifying node (e.g., log rotation on all nodes).
- Otherwise, it is placed on a single node via the standard placement pipeline.

### 5.3 Rolling Deploys

When an App's image changes (new deploy via CLI, API, or Lettuce GitOps), Meat orchestrates a rolling deployment:

```
For each instance (old version) in the App:
  1. Schedule a NEW instance (new version) on the same node.
     - Host ports are dynamically allocated, so old and new coexist.
  2. Wait for the new instance to pass health checks (up to health_timeout).
     - If health check fails:
       a. If auto_rollback = false: HALT. Log the failure. Keep upgraded
          instances on new version, remaining on old version.
       b. If auto_rollback = true: revert all already-upgraded instances
          back to the old version using the same rolling mechanism.
  3. Tell Wrapper to add the new instance to the routing pool.
  4. Tell Wrapper to remove the old instance from the routing pool.
  5. Wait for drain_timeout (default 30s) to allow in-flight requests to complete.
  6. Stop the old instance. Release its host port.
  7. Move to the next instance.
```

The `max_surge` parameter controls how many extra instances can exist simultaneously during the transition. With `max_surge = 1` (default), only one node is being transitioned at a time. With `max_surge = 2`, two nodes can be in transition simultaneously, cutting deploy time roughly in half at the cost of temporarily using more resources.

Meat tracks the rolling deploy as a `RollingDeployState` in memory (and checkpointed to Raft after each step). If the leader fails mid-deploy, the new leader reconstructs the deploy state from the Raft log and resumes from the last committed step.

#### Dependency Ordering

Jobs with `run_before = ["app.api"]` create a deploy dependency. When a deploy includes both the job and the app:

1. Meat schedules the job and waits for it to complete.
2. Only after successful job completion does Meat begin the rolling deploy of the dependent App.
3. If the job fails, the deploy is halted and an alert fires.

Dependencies are expressed as a DAG. Meat performs a topological sort at deploy time and executes stages in dependency order.

### 5.4 Autoscaling Logic

The autoscaler runs as a periodic task within Meat (default interval: 30 seconds).

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
- **Scale down:** Meat selects replicas to remove using inverse scoring (remove from the lowest-scoring node first, to reclaim the least-efficient placements). Removed instances go through the drain flow (Wrapper removes from routing, wait for drain_timeout, stop).

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

### 5.6 Namespace Quota Enforcement

Quotas are enforced at scheduling time (admission check), not retroactively. Before running the placement pipeline, Meat checks:

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

---

## 6. Configuration

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
| `scheduler.scoring.spread_weight` | `10` | Weight for anti-affinity spread (0-100). |
| `scheduler.scoring.stability_weight` | `5` | Weight for node uptime/stability (0-100). |

### Batch Jobs

| Knob | Default | Description |
|------|---------|-------------|
| `scheduler.batch.report_interval` | `5s` | How often nodes report batch completion status. |
| `scheduler.batch.max_allocation_size` | `10000` | Maximum instances in a single batch allocation to one node. |
| `scheduler.batch.reallocation_delay` | `10s` | Delay before re-allocating failed instances to other nodes. |

### Rolling Deploys

| Knob | Default | Description |
|------|---------|-------------|
| `defaults.app.deploy.strategy` | `"rolling"` | Deploy strategy. Currently only `rolling` is supported. |
| `defaults.app.deploy.max_surge` | `1` | Default max extra instances during rolling deploy. |
| `defaults.app.deploy.drain_timeout` | `"30s"` | Default drain timeout for connection draining. |
| `defaults.app.deploy.health_timeout` | `"60s"` | Default timeout for health checks on new instances. |
| `defaults.app.deploy.auto_rollback` | `false` | Whether to auto-revert all instances on health failure. |

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
| `network.port_range` | `"10000-60000"` | Ephemeral port range for host port allocation. |

---

## 7. Failure Modes

### 7.1 Node Failure During Batch Job Execution

**Scenario:** A node executing batch job instances becomes unreachable mid-execution.

**Detection:** Mustard gossip detects the node as unreachable within seconds. The hierarchical reporting tree detects missed reports at the aggregation interval.

**Response:**

1. Meat marks all `BatchAllocation` entries assigned to the failed node as `PartiallyFailed`.
2. The number of instances that had not yet reported completion is computed: `remaining = allocation.instance_count - last_report.completed - last_report.failed`.
3. Meat re-allocates these `remaining` instances to other eligible nodes via a new `BatchAllocation`.
4. Already-completed instances are not re-run (completion reports are idempotent and stored in the Raft log).

**Edge case:** If the node was mid-execution and rejoins before the re-allocation, the node's Bun agent will report its completed instances. Meat deduplicates: if a re-allocated instance completes on both the original and new node, the second completion is ignored.

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

- **New deploys:** Rejected with a clear error:
  ```
  Error: namespace "team-backend" CPU quota exhausted
    allocated: 7800m / limit: 8000m
    requested: 500m (app.new-service, 2 replicas)
    would exceed quota by 300m
  ```
- **Autoscale events:** The autoscaler's scale-up is blocked. The scale action is logged as `QuotaBlocked`, an alert fires, and the autoscaler retries at the next evaluation interval. The existing replicas continue running -- quota exhaustion does not cause running workloads to be terminated.

### 7.4 Leader Failure Mid-Deploy

**Scenario:** The leader fails while a rolling deploy is in progress (some instances upgraded, others not).

**Response:**

1. Raft elects a new leader.
2. The new leader enters the learning period, collects state reports.
3. It loads the `RollingDeployState` from the Raft log (checkpointed after each step).
4. It detects the partial deploy: some instances are running the new version, others the old.
5. It resumes the rolling deploy from the last committed step.

If `auto_rollback = true` and the health check of the in-progress step was pending when the leader failed, the new leader restarts the health check timer from zero (conservative approach -- it does not assume the previous leader's timer was accurate).

### 7.5 Cascading Node Failures

**Scenario:** Multiple nodes fail simultaneously (rack failure, network partition).

**Response:**

1. For each failed node's App instances, Meat attempts to reschedule to surviving nodes.
2. If surviving capacity is insufficient, some replicas remain unscheduled. An alert fires immediately.
3. Meat prioritizes rescheduling by workload criticality: Apps with fewer remaining healthy replicas are rescheduled first.
4. Batch jobs on failed nodes are re-allocated as described in 7.1.

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
- **Batch parallelism limits:** The `parallelism` field caps the maximum concurrent instances for a batch job. Meat enforces this limit and does not allocate more than `parallelism` instances across all nodes combined.
- **Rate limiting:** The leader's API server rate-limits deploy and job submission requests per authenticated identity (configurable, default 100 requests/minute). This prevents a compromised CI pipeline from flooding the scheduler.

### 8.3 Process Workload Escalation

**Threat:** A process workload (exec/script job) attempts to escape its sandbox.

**Mitigation:**

- Process workloads require explicit `admin` or `host-exec` permission.
- Operators must configure a binary allowlist in `node.toml`. Meat's Filter phase rejects process workloads targeting nodes without the required binary in the allowlist.
- Even after scheduling, Bun enforces cgroup isolation, PID namespace, restricted mount namespace, seccomp profile, and the dedicated `burger` user.

### 8.4 Scheduling Decision Replay

**Threat:** An attacker replays an old scheduling decision to start a workload that should no longer be running.

**Mitigation:**

- Each `SchedulingDecision` includes a monotonically increasing `decision_id` and a `spec_version`. Bun agents reject decisions with a `decision_id` lower than or equal to the last one they processed for a given App.
- Decisions are also committed to the Raft log, which has its own monotonic term and index. Replay of old Raft entries is prevented by the Raft protocol itself.

---

## 9. Performance

### 9.1 Throughput Targets

| Metric | Target | Measured (12-node bench) |
|--------|--------|--------------------------|
| Sustained job scheduling rate | >= 1,150 jobs/sec | 2,847 jobs/sec |
| Peak scheduling rate (burst) | -- | 4,210 jobs/sec |
| Scheduling latency p50 | < 5ms | 0.8ms |
| Scheduling latency p99 | < 20ms | 4.2ms |
| Scheduling latency p99.9 | < 50ms | 12.1ms |
| Queue depth at sustained rate | < 100 | 3 jobs |

These numbers reflect the benchmark methodology described in the whitepaper (Section 19): a batch job with `parallelism = 10000` is submitted, and nodes immediately complete instances (no actual work), measuring pure scheduling overhead.

### 9.2 Latency Bounds

- **App placement decision:** < 5ms for a single replica on a 10,000-node cluster. The Filter phase is O(N) in nodes, but is optimised by maintaining a pre-filtered index of nodes by label combination. The Score phase runs only on filtered candidates (typically 10-1000 nodes for constrained workloads).
- **Batch allocation:** < 20ms for partitioning 10,000 instances across eligible nodes. This is a single Raft commit, not per-instance.
- **Rolling deploy step:** Dominated by health_timeout (default 60s), not scheduler overhead. The scheduling overhead per step is < 10ms.
- **Autoscale evaluation:** < 5ms per App. With 1,000 autoscaled Apps, the full evaluation cycle completes in < 5 seconds (well within the 30-second default interval).

### 9.3 Memory Budget at 10,000 Nodes

```
NodeCapacity per node:   ~2 KB (labels, resource counters, image set)
10,000 nodes:            ~20 MB

Per-App scheduling state: ~500 bytes
5,000 Apps (500/node avg): ~2.5 MB

Batch tracking state:    ~100 bytes per active allocation
10,000 active allocations: ~1 MB

Rolling deploy state:    ~2 KB per in-progress deploy
100 concurrent deploys:  ~200 KB

Autoscale state:         ~4 KB per App (metric sample window)
1,000 autoscaled Apps:   ~4 MB

Total Meat memory:      ~28 MB (at 10,000 nodes, peak)
```

This fits comfortably within a typical server's memory. The 312 MB total Bun agent memory (at 500 apps) noted in the whitepaper benchmarks includes all Bun subsystems, not just Meat.

### 9.4 CPU Budget

Meat's scheduling loop is designed to consume < 15% of a single core at sustained throughput. At the 1,150 jobs/sec target:

- Each scheduling decision involves: one Filter pass (~50us), one Score pass (~20us), one Select (~1us), one Raft commit (~200us for the leader's local append; replication is async for scheduling decisions that are not App deploys).
- Total: ~270us per decision. At 1,150 decisions/sec = ~310ms of CPU per second = 31% of one core.
- With core pinning at > 1,000 nodes, this is isolated from other Bun tasks.

In practice, the batch model means most scheduling decisions are batch allocations (one per node per batch, not one per instance), so the actual decision rate for 100M jobs/day is closer to 50-200 allocation decisions per second, well within budget.

---

## 10. Testing Strategy

### 10.1 Scheduler Simulation

Meat includes a deterministic simulation mode that can be driven without a real cluster. The simulator replaces the Raft log, Mustard gossip, and reporting tree with in-memory fakes.

```rust
#[cfg(test)]
mod simulation {
    use super::*;

    /// A simulated cluster with configurable nodes and workloads.
    struct SchedulerSimulation {
        scheduler: MeatScheduler,
        nodes: Vec<SimulatedNode>,
        clock: FakeClock,
        raft_log: Vec<RaftEntry>,
    }

    impl SchedulerSimulation {
        /// Create a simulation with N nodes, each with the given capacity.
        fn new(node_count: usize, capacity: Resources) -> Self { /* ... */ }

        /// Add labels to a subset of nodes.
        fn label_nodes(&mut self, range: Range<usize>, labels: BTreeMap<String, String>) { /* ... */ }

        /// Submit an app spec and run the scheduler until all replicas are placed.
        fn deploy_app(&mut self, spec: AppSpec) -> Vec<SchedulingDecision> { /* ... */ }

        /// Submit a batch job and run until all instances complete.
        fn run_batch(&mut self, spec: JobSpec, instances: u32) -> Vec<BatchAllocation> { /* ... */ }

        /// Simulate a node failure and verify rescheduling.
        fn fail_node(&mut self, node_id: &NodeId) { /* ... */ }

        /// Advance the simulated clock and process pending events.
        fn advance(&mut self, duration: Duration) { /* ... */ }

        /// Assert that all replicas of an App are placed on distinct nodes.
        fn assert_spread(&self, app_id: &AppId) { /* ... */ }

        /// Assert that all replicas are on nodes matching the given labels.
        fn assert_labels(&self, app_id: &AppId, labels: &BTreeMap<String, String>) { /* ... */ }
    }
}
```

Key simulation test scenarios:

- **Bin-packing correctness:** Deploy 100 apps with varied resource requirements across 10 nodes. Verify that utilisation is balanced and no node is over-committed.
- **Required label enforcement:** Deploy an app requiring `gpu_model = "a100"` to a cluster with mixed GPU types. Verify placement only on A100 nodes.
- **Preferred label fallback:** Deploy an app preferring `storage = "ssd"` when SSD nodes are full. Verify fallback to non-SSD nodes.
- **Daemon mode join/leave:** Deploy a `replicas = "*"` app, then add and remove nodes. Verify instance count tracks node count.
- **Quota enforcement:** Configure a namespace quota and attempt to exceed it. Verify rejection.
- **Batch throughput:** Simulate 100,000 job instances across 100 nodes and verify allocation completes in < 1 second of simulated time.
- **Autoscaler stability:** Feed a sinusoidal load pattern into the autoscaler and verify it does not flap (the smoothing window and cooldown prevent oscillation).
- **Rolling deploy with failure:** Start a rolling deploy where the new version fails health checks on the third replica. Verify halt (without auto_rollback) and full revert (with auto_rollback).

### 10.2 Integration Tests

The following tests from the built-in `relish test` suite exercise Meat against a live cluster:

| Test | Description | Expected Duration |
|------|-------------|-------------------|
| Deploy 3 replicas, verify distinct nodes | Basic placement spread | 1.8s |
| Deploy daemon app, verify on all nodes | `replicas = "*"` mode | 2.4s |
| Deploy with required labels | Hard constraint enforcement | 1.6s |
| Deploy with preferred labels | Soft constraint with fallback | 1.9s |
| Namespace quota rejection | Quota enforcement | 0.4s |
| Rolling deploy zero-downtime | Continuous health probe during deploy | 8.2s |
| Deploy broken image, auto-rollback | Rollback mechanism | 12.4s |
| Job runs before app starts | Dependency ordering | 4.1s |
| Failed job retries | Retry logic | 3.8s |

### 10.3 Chaos Tests

From the `relish test --chaos` suite:

| Test | Description | Expected Duration |
|------|-------------|-------------------|
| Kill leader mid-deploy | Deploy completes after election | 8.4s |
| Kill node, verify rescheduling | Replicas move to surviving nodes | 12.3s |
| Drain node, zero-downtime migration | Graceful node removal | 9.8s |
| Kill 2 of 3 replicas simultaneously | Service stays up | 4.8s |
| Rapid leader elections (3 in 30s) | Cluster stabilizes | 22.4s |

### 10.4 Benchmark Tests

The `relish bench` scheduler throughput test:

1. Deploys a batch job with `parallelism = 10000`.
2. Nodes immediately complete (zero-work stub).
3. Measures sustained and peak scheduling rates.
4. Ramps submission rate until p99 latency exceeds 50ms to find the saturation point.

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
- Nomad's batch scheduler queues individual allocations. Meat's delegated batch model pushes entire batches to nodes, keeping the leader's hot path O(nodes) rather than O(jobs).

**Reference:** [Nomad Scheduler Design](https://developer.hashicorp.com/nomad/docs/concepts/scheduling/scheduling), [Nomad Architecture](https://developer.hashicorp.com/nomad/docs/concepts/architecture)

### Google Borg

Borg is Google's internal cluster manager. Its scheduler handles both long-running "production" workloads and batch workloads in a single system. Borg uses a priority-based model where production workloads preempt batch workloads when resources are scarce.

**What we borrow:** Borg's batch model -- the insight that high-throughput batch scheduling requires delegation rather than centralized per-task decisions. Borg assigns "allocs" (resource reservations) and lets tasks fill them. Meat's `BatchAllocation` model is a simplified version of this concept.

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
| `std::collections::BinaryHeap` | (stdlib) | Priority queue for the cron scheduler (next-fire-time ordering). |
| `dashmap` | 6.x | Lock-free concurrent hash map for the cluster state cache. Multiple async tasks read node state concurrently; only the reporting tree handler writes. |
| `serde` + `bincode` | 1.x / 2.x | Serialisation for Raft log entries (`SchedulingDecision`, `BatchAllocation`). Bincode for compactness on the hot path; JSON for API responses. |
| `cron` | 0.13.x | Cron expression parsing for scheduled jobs. |
| `parking_lot` | 0.12.x | Fast mutex/rwlock for the autoscaler state map (low contention, but must be held briefly during metric evaluation). |
| `metrics` | 0.24.x | Instrumentation of scheduler internals (decisions/sec, latency histograms, queue depth). Fed into Mayo. |
| `tracing` | 0.1.x | Structured logging for scheduler decisions, used for debugging and audit trails. |
| `core_affinity` | 0.8.x | CPU core pinning for the scheduler task when the cluster exceeds 1,000 nodes. |
| `rand` | 0.8.x | Tie-breaking during node selection when scores are equal (optional jitter mode). |
| `smallvec` | 1.x | Stack-allocated small vectors for filter results (avoids heap allocation when candidate sets are small). |

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

Currently, bin-packing is the primary strategy and spread is a scoring component. Some workloads (latency-sensitive services) benefit from a spread-first strategy that distributes replicas across as many nodes as possible, even if this reduces density. Should Meat support a per-App `strategy = "spread"` that inverts the scoring weights? **Current decision: under consideration. The current spread scoring weight (10/100) provides mild anti-affinity; a dedicated spread mode would set it to 50+ and reduce bin-packing weight.**

### 13.5 Topology-Aware Scheduling

Should Meat understand rack topology (from node labels like `rack = "rack-3"`) and enforce spread across racks, not just across nodes? This would protect against rack-level failures. **Current decision: under consideration. Rack-aware spread could be implemented as an additional scoring dimension or as a hard constraint (e.g., `placement.spread_by = "rack"`).**

### 13.6 Autoscaler Custom Metrics

The autoscaler currently supports `cpu`, `memory`, and a `Custom(String)` metric path that queries Mayo. The interface for custom metrics is not fully specified. **Open question: should custom metrics be PromQL expressions evaluated by Mayo, or simple metric-name + aggregation-function pairs?** PromQL is more powerful but adds a dependency on Mayo's query engine. Simple pairs are easier to implement and validate.

### 13.7 Scheduling Latency SLO Enforcement

Should Meat provide a hard guarantee that scheduling decisions complete within a latency bound (e.g., 50ms)? Currently, latency is best-effort and depends on cluster size and Raft commit speed. An SLO enforcement mode could shed load (reject submissions) when latency exceeds the bound. **Current decision: under consideration for v2.**
