# Bun Agent & Grill Container Runtime -- Design Document

**Component:** Bun (per-node agent/daemon), Grill (container runtime interface)
**Scope:** Container lifecycle, process workload isolation, node configuration (`node.toml`), self-upgrade mechanism
**Status:** Draft
**Source:** Whitepaper sections 5 (Core Concepts), 17 (Process Workloads), 7 (Cluster Architecture), 9 (Networking), 11 (Security), 13 (Deployments), 18 (Fault Injection), 6 (Node Configuration), 20 (Self-Upgrades), 19 (Testing)

---

## 1. Overview

Bun is the per-node agent that runs on every node in a Reliaburger cluster. It is not a separate binary -- it is the single `reliaburger` binary operating in agent mode. Every node runs the same binary, and every node runs Bun. There is no distinction between control plane nodes and worker nodes at the binary level; nodes acquire additional responsibilities (leader, council member, metrics aggregator) dynamically at runtime.

Bun's responsibilities on every node:

- **Container management (Grill):** Start, stop, health-check, and monitor OCI containers via the Grill abstraction layer over containerd/runc.
- **Process workload management:** Start, stop, health-check, and isolate non-container workloads (host binaries and inline scripts) using Linux namespaces and cgroups.
- **Port allocation:** Allocate ephemeral host ports from a configurable range (default 10000-60000) and map them to container-internal ports.
- **eBPF service discovery (Onion):** Load and maintain the Onion eBPF programs and kernel maps for socket-level service discovery and firewall enforcement.
- **Log collection (Ketchup):** Capture stdout/stderr from all workloads and feed them into the structured log pipeline.
- **Metrics collection (Mayo):** Scrape per-workload metrics endpoints and feed them into the time-series store.
- **Image registry node (Pickle):** Participate in the distributed, content-addressed image registry.
- **Gossip participation (Mustard):** Participate in the cluster-wide gossip mesh for failure detection and metadata propagation.
- **Ingress proxy (Wrapper):** Serve external HTTP/HTTPS traffic with TLS termination, routing, and health-check-aware load balancing.
- **Web UI (Brioche):** Serve the compiled-in web dashboard.
- **Secret decryption:** Decrypt `ENC[AGE:...]` values in memory and inject them as environment variables.
- **Certificate management:** Auto-renew node certificates (1-year lifetime), issue workload identity certificates via CSR to council members (1-hour lifetime, rotated every 30 minutes).
- **Volume management:** Create, snapshot, and enforce size limits on local persistent volumes.
- **Fault injection (Smoker):** Execute fault injection commands by writing to eBPF maps and cgroup controls.
- **Self-upgrade:** Stage, verify, and apply rolling binary upgrades with automatic rollback.
- **nftables management:** Maintain perimeter firewall rules (cluster boundary, management access, egress allowlists).
- **GPU detection:** Detect GPUs at startup via NVML and report them as schedulable resources.

Grill is Bun's container runtime interface -- the abstraction layer between Bun's workload management logic and the underlying container runtime (containerd + runc). Grill handles OCI image unpacking, container creation, network namespace setup, cgroup configuration, port mapping, and stdio stream attachment. The name follows the Reliaburger convention: Grill is where containers get cooked.

**Key design decisions:**

1. **Single binary, no sidecar model.** Bun is not a separate process from the orchestrator -- it IS the orchestrator on this node. All subsystems (Grill, Onion, Ketchup, Mayo, Pickle, Mustard, Wrapper, Brioche) run as async tasks within the same Tokio runtime. This eliminates IPC overhead and version compatibility concerns between components.

2. **Containers survive Bun restarts.** Containers are managed by containerd/runc, which is independent of Bun. When Bun restarts (including during self-upgrade), it reconnects to running containers by querying the container runtime. Applications are never interrupted by Bun lifecycle events.

3. **Process workloads are first-class.** Process workloads (host binaries and inline scripts) receive the same isolation primitives as containers (cgroups, PID namespace, network namespace, mount namespace) minus the filesystem image. From the cluster's perspective, they are indistinguishable from container workloads in terms of scheduling, health checking, service discovery, and metrics.

4. **Sub-millisecond per-app overhead.** At the design goal of 500 apps per node, Bun must have sub-millisecond per-app overhead. This drives the choice of Rust (no GC pauses), eBPF (kernel-space interception, not userspace proxying), and direct cgroup/namespace manipulation (no shim processes per container).

5. **Deny-by-default security.** Process workloads require an explicit binary allowlist in `node.toml`. Capabilities are restricted. Seccomp profiles block dangerous syscalls. The `burger` unprivileged user is used for all workloads.

---

## 2. Dependencies

| Component | Dependency Type | Why |
|-----------|----------------|-----|
| **containerd** | Runtime (external process) | OCI container lifecycle, image management, container state persistence across Bun restarts |
| **runc** | Runtime (external process, via containerd) | Low-level container execution (namespaces, cgroups, seccomp) |
| **Linux kernel 5.7+** | Hard requirement | eBPF socket-level interception (`sock_ops`, `connect4`, `sk_msg`), cgroup v2, BPF CO-RE |
| **Mustard (gossip)** | Internal subsystem | Failure detection, leader discovery, cluster membership, metadata propagation (protocol version, node labels) |
| **Patty (scheduler)** | Via leader node | Receives scheduling decisions (which workloads to run). On the leader node, Patty runs as a co-located async task. On worker nodes, scheduling decisions arrive via the reporting tree. |
| **Raft (council)** | Via council members | Committed state (app specs, secrets, config). Worker nodes receive state via the reporting tree, not directly from Raft. |
| **Onion (eBPF)** | Internal subsystem | Bun loads and manages the Onion eBPF programs and kernel maps. Onion depends on Bun for map updates when workloads start/stop or health checks change state. |
| **Pickle (image registry)** | Internal subsystem | Image pull/push, binary distribution during self-upgrade. Bun participates as a Pickle storage node. |
| **Sesame (PKI)** | Via council members | Node certificate issuance (at join), workload certificate issuance (CSR model), CA trust chain distribution. |
| **NVML** | Optional runtime library | GPU detection and enumeration. Auto-detected at startup. If absent, GPU scheduling is disabled on this node. |
| **nftables** | Runtime (kernel + userspace) | Perimeter firewall rules. Bun manages a dedicated nftables table. |
| **Btrfs / ext4 / xfs** | Filesystem | Volume management. Btrfs preferred (subvolume quotas, instant CoW snapshots). ext4/xfs supported with loop-mount fallback. |

---

## 3. Architecture

### 3.1 Internal Structure

Bun runs as a single Tokio async runtime with multiple subsystem tasks. Each subsystem is a long-lived `tokio::task` that communicates with others via channels and shared state.

```
┌─────────────────────────────────────────────────────────────────┐
│  Bun Process (single reliaburger binary)                        │
│                                                                  │
│  ┌──────────────────┐  ┌──────────────────┐                    │
│  │  Grill           │  │  ProcessManager   │                    │
│  │  (container CRI) │  │  (process wklds)  │                    │
│  └────────┬─────────┘  └────────┬──────────┘                    │
│           │                     │                                │
│  ┌────────▼─────────────────────▼──────────┐                    │
│  │           WorkloadSupervisor            │                    │
│  │  (unified lifecycle, health, events)     │                    │
│  └────────┬───────────┬───────────┬────────┘                    │
│           │           │           │                              │
│  ┌────────▼───┐ ┌─────▼─────┐ ┌──▼────────┐                    │
│  │HealthChecker│ │PortAllocator│ │CgroupMgr │                    │
│  └────────────┘ └───────────┘ └───────────┘                    │
│                                                                  │
│  ┌────────────┐ ┌────────────┐ ┌────────────┐ ┌────────────┐  │
│  │   Onion    │ │  Ketchup   │ │    Mayo    │ │   Pickle   │  │
│  │  (eBPF)   │ │  (logs)    │ │ (metrics)  │ │  (images)  │  │
│  └────────────┘ └────────────┘ └────────────┘ └────────────┘  │
│                                                                  │
│  ┌────────────┐ ┌────────────┐ ┌────────────┐ ┌────────────┐  │
│  │  Mustard   │ │  Wrapper   │ │  Brioche   │ │  Smoker    │  │
│  │ (gossip)   │ │ (ingress)  │ │   (UI)     │ │ (faults)   │  │
│  └────────────┘ └────────────┘ └────────────┘ └────────────┘  │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  NodeConfig (parsed from /etc/reliaburger/node.toml)    │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  UpgradeManager (staging, verification, exec, rollback) │    │
│  └─────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────┘
```

### 3.2 Data Flow

**Inbound (workload scheduling):**

```
Leader (Patty)
  → Reporting tree (council member parent)
    → Bun on this node
      → WorkloadSupervisor
        → Grill (container) or ProcessManager (process workload)
          → containerd/runc or clone()/exec() with namespaces
```

**Outbound (state reporting):**

```
Workload event (start, stop, health change, OOM, restart)
  → WorkloadSupervisor
    → Local event log
    → Reporting tree parent (council member)
      → Council → Raft state (for cluster-wide queries)
    → Onion service map update (eBPF kernel map)
    → Ketchup log entry
    → Mayo metric update
```

**Health check loop:**

```
HealthChecker (periodic per workload)
  → HTTP GET to workload's health endpoint (via container's network namespace)
  → Result → WorkloadSupervisor
    → If state changed: update Onion service map, emit event, notify reporting tree
    → If unhealthy beyond threshold: trigger restart via Grill/ProcessManager
```

### 3.3 Key Abstractions

**Grill** abstracts the container runtime. It communicates with containerd over a Unix socket using the containerd gRPC API. Grill is responsible for:

- Translating `AppSpec` into OCI container configurations
- Creating and configuring network namespaces with port mappings
- Setting up cgroups (CPU, memory, GPU via device allow-listing)
- Mounting volumes, config files, workload identity certs, and secrets
- Attaching to container stdio streams for log capture
- Reconnecting to running containers after Bun restart

**ProcessManager** handles non-container workloads. It uses direct Linux syscalls (`clone()`, `unshare()`, `mount()`, `pivot_root()`) to create isolated execution environments without a container image. ProcessManager is responsible for:

- Validating binaries against the allowlist in `node.toml`
- Creating restricted mount namespaces (allow-listed paths only)
- Writing inline scripts to temporary files and marking them executable
- Spawning processes with PID, network, UTS, and mount namespace isolation
- Applying the same cgroup limits and seccomp profiles as containers
- Running processes as the `burger` unprivileged user

**WorkloadSupervisor** is the unified control plane for both container and process workloads. It does not care whether a workload is a container or a process -- it manages the lifecycle state machine, health checking, restart policies, event emission, and service map updates identically for both types.

---

## 4. Data Structures

### 4.1 Core Types

```rust
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Unique identifier for an app instance on this node.
/// Format: "{app_name}-{replica_index}" e.g. "web-3"
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct InstanceId(pub String);

/// Unique identifier for a workload across the cluster.
/// Combines namespace + app name + instance index.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct WorkloadId {
    pub namespace: String,
    pub app_name: String,
    pub instance: u32,
}

/// The specification for an application, as received from the scheduler.
/// This is the deserialised form of the TOML app declaration after
/// scheduling decisions have been applied.
#[derive(Debug, Clone)]
pub struct AppSpec {
    pub name: String,
    pub namespace: String,
    pub workload_source: WorkloadSource,
    pub port: Option<u16>,
    pub health: Option<HealthSpec>,
    pub cpu: ResourceRange,
    pub memory: ResourceRange,
    pub gpu: Option<u32>,
    pub env: HashMap<String, EnvValue>,
    pub config_files: Vec<ConfigFileSpec>,
    pub volume: Option<VolumeSpec>,
    pub init_containers: Vec<InitContainerSpec>,
    pub allow_paths: Vec<MountPath>,
    pub capabilities: Vec<String>,
    pub capture: bool,
    pub deploy: DeploySpec,
    pub firewall: FirewallSpec,
    pub egress: EgressSpec,
    pub autoscale: Option<AutoscaleSpec>,
    pub placement: PlacementSpec,
}

/// Determines whether this workload runs as a container or a process.
#[derive(Debug, Clone)]
pub enum WorkloadSource {
    /// Container workload: pull and run an OCI image.
    Image {
        image: String,
        command: Option<Vec<String>>,
    },
    /// Process workload: run a pre-installed binary from the host.
    Exec {
        binary: PathBuf,
        args: Vec<String>,
    },
    /// Process workload: run an inline script written to a temp file.
    Script {
        content: String,
        interpreter: Option<String>, // inferred from shebang if absent
    },
}

/// A resource range like "100m-500m" for CPU or "128Mi-512Mi" for memory.
/// `request` is the guaranteed minimum. `limit` is the hard ceiling.
#[derive(Debug, Clone, Copy)]
pub struct ResourceRange {
    pub request: u64, // millicores for CPU, bytes for memory
    pub limit: u64,
}

/// Health check specification.
#[derive(Debug, Clone)]
pub struct HealthSpec {
    pub path: String,                         // HTTP path, e.g. "/healthz"
    pub interval: Duration,                   // default: 10s
    pub timeout: Duration,                    // default: 5s
    pub unhealthy_threshold: u32,             // default: 3
    pub healthy_threshold: u32,               // default: 1
    pub initial_delay: Duration,              // default: 0s
}

/// Result of a single health check probe.
#[derive(Debug, Clone)]
pub struct HealthCheckResult {
    pub instance_id: InstanceId,
    pub timestamp: u64,                       // unix timestamp nanos
    pub status: HealthStatus,
    pub http_status: Option<u16>,
    pub latency: Duration,
    pub body_snippet: Option<String>,         // first 256 bytes on failure
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Healthy,
    Unhealthy,
    Timeout,
    ConnectionRefused,
    Unknown,
}

/// An environment variable value, which may be encrypted.
#[derive(Debug, Clone)]
pub enum EnvValue {
    Plain(String),
    Encrypted(String), // ENC[AGE:...] -- decrypted at injection time
}

/// A config file to mount into the workload's filesystem.
#[derive(Debug, Clone)]
pub struct ConfigFileSpec {
    pub path: PathBuf,        // mount path inside the container
    pub content: ConfigFileSource,
}

#[derive(Debug, Clone)]
pub enum ConfigFileSource {
    Inline(String),
    GitRef(String),           // relative path within the git repo
}

/// A local persistent volume.
#[derive(Debug, Clone)]
pub struct VolumeSpec {
    pub path: PathBuf,        // mount path inside the container
    pub size: u64,            // bytes
    pub snapshot: Option<SnapshotSchedule>,
}

/// An additional host path to mount into a process workload.
#[derive(Debug, Clone)]
pub struct MountPath {
    pub host_path: PathBuf,
    pub read_write: bool,     // false = read-only (default)
}

/// Init container specification.
#[derive(Debug, Clone)]
pub struct InitContainerSpec {
    pub image: Option<String>,   // inherits parent app image if None
    pub command: Vec<String>,
}

/// Deployment strategy.
#[derive(Debug, Clone)]
pub struct DeploySpec {
    pub strategy: DeployStrategy,
    pub max_surge: u32,
    pub drain_timeout: Duration,
    pub health_timeout: Duration,
    pub auto_rollback: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum DeployStrategy {
    Rolling,
}

/// Firewall rules for this app.
#[derive(Debug, Clone)]
pub struct FirewallSpec {
    pub allow_from: Vec<String>,  // app names that may connect
}

/// Egress allowlist.
#[derive(Debug, Clone)]
pub struct EgressSpec {
    pub allow: Vec<String>,       // "host:port" entries
}

/// Autoscale configuration.
#[derive(Debug, Clone)]
pub struct AutoscaleSpec {
    pub metric: String,
    pub target: u32,
    pub min: u32,
    pub max: u32,
}

/// Placement constraints.
#[derive(Debug, Clone)]
pub struct PlacementSpec {
    pub required: HashMap<String, String>,
    pub preferred: HashMap<String, String>,
}
```

### 4.2 Container State Machine

```rust
/// The lifecycle state of a workload instance (container or process).
/// This state machine is identical for both workload types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    /// Spec received from scheduler. Image pull or binary validation
    /// has not yet started.
    Pending,

    /// Image is being pulled (container) or binary is being validated
    /// against the allowlist (process workload).
    Preparing,

    /// Init containers are running sequentially. If any fails, the
    /// workload transitions to Failed.
    Initialising,

    /// The main process is starting. For containers: containerd has
    /// created the container and is executing the entrypoint. For
    /// process workloads: clone() has been called and the process
    /// is executing.
    Starting,

    /// The main process is running but the initial health check
    /// has not yet passed. The workload is not added to the service
    /// map and does not receive traffic.
    HealthWait,

    /// The workload is running and healthy. It is registered in the
    /// Onion service map and receives traffic.
    Running,

    /// The health check has failed `unhealthy_threshold` consecutive
    /// times. The workload is removed from the Onion service map
    /// (no new traffic). Existing connections drain.
    Unhealthy,

    /// The workload is being stopped. For rolling deploys: the old
    /// instance is draining connections before termination. For
    /// scale-down: a graceful SIGTERM has been sent.
    Stopping,

    /// The workload has exited (process exit, container stop, or
    /// SIGKILL after drain timeout). Resources are being cleaned up
    /// (cgroup removal, port release, temp file deletion).
    Stopped,

    /// The workload failed to start, failed init containers, or
    /// crashed and exceeded the restart backoff limit.
    Failed,
}
```

**State machine diagram:**

```
                          ┌──────────────────────────────────────┐
                          │           Pending                     │
                          └──────────────┬───────────────────────┘
                                         │
                                         ▼
                          ┌──────────────────────────────────────┐
                          │          Preparing                    │
                          │  (image pull / binary validation)     │
                          └──────────┬───────────┬───────────────┘
                                     │           │
                              success│           │failure
                                     ▼           ▼
                          ┌─────────────────┐  ┌──────┐
                          │  Initialising    │  │Failed│
                          │ (init containers)│  └──────┘
                          └────┬────────┬───┘
                               │        │
                        success│        │failure
                               ▼        ▼
                          ┌──────────┐ ┌──────┐
                          │ Starting │ │Failed│
                          └────┬─────┘ └──────┘
                               │
                               ▼
                          ┌──────────────────────────────────────┐
                          │          HealthWait                   │
                          │  (initial delay + first health pass)  │
                          └──────────┬──────────┬────────────────┘
                                     │          │
                          health pass│          │timeout / crash
                                     ▼          ▼
                          ┌──────────┐    ┌──────────┐
                     ┌───▶│ Running  │    │  Failed  │
                     │    └────┬─────┘    └──────────┘
                     │         │
                     │         │ health fails N times
                     │         ▼
                     │    ┌──────────┐
                     │    │Unhealthy │
                     │    └────┬─────┘
                     │         │
                     │         ├── restart? ──▶ Stopping ──▶ Stopped ──▶ Pending
                     │         │                                         (restart)
                     │         │ health recovers
                     │         │
                     └─────────┘
                          │
               stop/scale-down/deploy
                          │
                          ▼
                    ┌──────────┐
                    │ Stopping │
                    │ (drain)  │
                    └────┬─────┘
                         │
                         ▼
                    ┌──────────┐
                    │ Stopped  │
                    └──────────┘
```

### 4.3 Process Workload Types

```rust
/// A process workload -- either a long-running app or a job.
#[derive(Debug, Clone)]
pub struct ProcessWorkload {
    pub id: WorkloadId,
    pub source: ProcessSource,
    pub isolation: IsolationConfig,
    pub state: ContainerState,  // same state machine as containers
    pub pid: Option<u32>,       // host PID of the spawned process
    pub cgroup_path: Option<PathBuf>,
    pub network_namespace: Option<PathBuf>,
    pub host_port: Option<u16>,
    pub started_at: Option<u64>,
    pub restart_count: u32,
}

#[derive(Debug, Clone)]
pub enum ProcessSource {
    Binary {
        path: PathBuf,
        args: Vec<String>,
    },
    Script {
        temp_path: PathBuf,  // where Bun wrote the script
        content_hash: [u8; 32], // SHA-256 of the script content
    },
}

/// Isolation primitives applied to a process workload.
#[derive(Debug, Clone)]
pub struct IsolationConfig {
    pub cpu: ResourceRange,
    pub memory: ResourceRange,
    pub gpu_devices: Vec<GpuDevice>,
    pub pid_namespace: bool,       // always true
    pub network_namespace: bool,   // always true
    pub mount_namespace: bool,     // always true
    pub uts_namespace: bool,       // always true
    pub user_namespace: bool,      // always false for process workloads
    pub seccomp_profile: SeccompProfile,
    pub capabilities: Vec<LinuxCapability>,
    pub run_as_user: String,       // "burger" (never root)

    /// Paths visible to the workload. Combines the default set
    /// with any `allow_paths` from the app spec.
    pub visible_mounts: Vec<MountEntry>,
}

/// A single mount in the process workload's mount namespace.
#[derive(Debug, Clone)]
pub struct MountEntry {
    pub source: PathBuf,
    pub target: PathBuf,
    pub read_only: bool,
    pub mount_type: MountType,
}

#[derive(Debug, Clone, Copy)]
pub enum MountType {
    Bind,   // bind mount from host
    Tmpfs,  // per-workload /tmp
    Device, // GPU device passthrough
}

#[derive(Debug, Clone)]
pub struct GpuDevice {
    pub index: u32,
    pub model: String,
    pub vram_bytes: u64,
    pub device_path: PathBuf,    // e.g. /dev/nvidia0
}

#[derive(Debug, Clone)]
pub enum SeccompProfile {
    Default, // blocks mount, reboot, kexec_load, init_module, etc.
    Custom(PathBuf),
}

#[derive(Debug, Clone, Copy)]
pub enum LinuxCapability {
    SysPtrace,
    DacOverride,
    Chown,
    SetUid,
    SetGid,
    // NET_ADMIN and NET_RAW are explicitly NOT available
}
```

### 4.4 Node Configuration

```rust
/// The complete parsed representation of /etc/reliaburger/node.toml.
/// Every field has a sensible default; the minimal config is just
/// `[cluster] join = ["addr:port"]`.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub node: NodeIdentity,
    pub cluster: ClusterConfig,
    pub storage: StoragePaths,
    pub resources: ResourceReservation,
    pub network: NetworkConfig,
    pub images: ImageConfig,
    pub logs: LogConfig,
    pub metrics: MetricsConfig,
    pub ingress: IngressConfig,
    pub process_workloads: ProcessWorkloadConfig,
    pub upgrades: UpgradeConfig,
}

#[derive(Debug, Clone)]
pub struct NodeIdentity {
    /// Node name. Default: hostname.
    pub name: String,
    /// Arbitrary key-value labels for placement constraints.
    /// `gpu = "true"` and `gpu_model = "<model>"` are added
    /// automatically by NVML detection.
    pub labels: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ClusterConfig {
    /// Addresses of existing cluster members to join.
    /// Empty for the first node (`relish init`).
    pub join: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct StoragePaths {
    pub data: PathBuf,     // default: /var/lib/reliaburger/data
    pub images: PathBuf,   // default: /var/lib/reliaburger/images
    pub logs: PathBuf,     // default: /var/lib/reliaburger/logs
    pub metrics: PathBuf,  // default: /var/lib/reliaburger/metrics
    pub volumes: PathBuf,  // default: /var/lib/reliaburger/volumes
}

#[derive(Debug, Clone)]
pub struct ResourceReservation {
    /// CPU reserved for system + Bun overhead. Not allocatable to workloads.
    pub reserved_cpu: u64,       // default: 500 millicores
    /// Memory reserved for system + Bun overhead.
    pub reserved_memory: u64,    // default: 512 MiB
    /// Disk reserved for system.
    pub reserved_disk: u64,      // default: 10 GiB
    /// Whether to auto-detect GPUs via NVML.
    pub gpu_enabled: bool,       // default: true
}

#[derive(Debug, Clone)]
pub struct NetworkConfig {
    /// IP address this node advertises to the cluster.
    /// Default: auto-detected from the default route interface.
    pub advertise_address: Option<IpAddr>,
    /// Ephemeral port range for container port mapping.
    pub port_range: PortRange,   // default: 10000-60000
}

#[derive(Debug, Clone, Copy)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

#[derive(Debug, Clone)]
pub struct ImageConfig {
    pub max_storage: u64,        // default: 50 GiB
    pub redundancy: u32,         // default: 2
    pub gc_retain_tags: u32,     // default: 10
    pub gc_retain_days: u32,     // default: 30
    pub pre_pull: bool,          // default: true
    pub external_registries: Vec<ExternalRegistry>,
}

#[derive(Debug, Clone)]
pub struct ExternalRegistry {
    pub host: String,
    pub username: Option<String>,
    pub password_secret: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LogConfig {
    pub retention_days: u32,              // default: 7
    pub compressed_retention_days: u32,   // default: 30
    pub max_storage: u64,                 // default: 20 GiB
}

#[derive(Debug, Clone)]
pub struct MetricsConfig {
    pub collection_interval: Duration,    // default: 10s
    pub retention_full: Duration,         // default: 24h
    pub retention_1m: Duration,           // default: 7d
    pub retention_1h: Duration,           // default: 90d
    pub max_storage: u64,                 // default: 5 GiB
}

#[derive(Debug, Clone)]
pub struct IngressConfig {
    pub enabled: bool,                    // default: true
    pub http_port: u16,                   // default: 80
    pub https_port: u16,                  // default: 443
    pub tls_acme_email: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProcessWorkloadConfig {
    /// Explicit list of binaries permitted to run as process workloads.
    /// When empty, process workloads are disabled on this node.
    pub allowed_binaries: Vec<String>,
    /// Whether glob patterns are permitted in allowed_binaries.
    /// Default: false. Glob patterns in allowed_binaries are rejected
    /// with an error unless this is true.
    pub allow_globs: bool,
}

#[derive(Debug, Clone)]
pub struct UpgradeConfig {
    /// External Ed25519 signing key for dual-signature verification.
    /// Required for network-based upgrades. Air-gapped (--binary)
    /// upgrades work without this.
    pub external_signing_key: Option<String>,
    /// Number of previous binary versions to retain on disk.
    pub retain_versions: u32,   // default: 3
    /// Release metadata endpoint URL.
    pub release_url: String,    // default: https://releases.reliaburger.dev/metadata.json
}
```

### 4.5 On-Disk Formats

**Binary versioning layout:**

```
/usr/local/bin/
  reliaburger                  -> reliaburger-v1.4.0  (symlink)
  reliaburger-v1.3.2           (previous version, kept for rollback)
  reliaburger-v1.4.0           (current version)
  reliaburger-v1.4.0.sig       (detached Ed25519 signature)
```

**Volume layout (Btrfs):**

```
/var/lib/reliaburger/volumes/
  {namespace}/
    {app_name}/
      current/                  (Btrfs subvolume, mounted into workload)
      snapshots/
        {app}-{timestamp}/      (Btrfs snapshot, read-only)
```

**Volume layout (ext4/xfs fallback):**

```
/var/lib/reliaburger/volumes/
  {namespace}/
    {app_name}/
      current/                  (loop-mounted sparse file, size-limited)
      snapshots/
        {app}-{timestamp}/      (directory copy)
```

**Workload identity mount (tmpfs, per workload):**

```
/var/run/reliaburger/identity/
  cert.pem                      (X.509 certificate, SPIFFE SAN)
  key.pem                       (private key)
  ca.pem                        (CA trust chain)
  token                         (JWT with workload claims)
```

### 4.6 Wire Protocols

**Bun <-> containerd:** gRPC over Unix socket (`/run/containerd/containerd.sock`). Uses the containerd v1 API (`containerd.services.containers.v1`, `containerd.services.tasks.v1`, etc.).

**Bun <-> reporting tree parent:** mTLS gRPC. Messages include:

- `WorkloadStateReport` -- periodic batch of workload states (instance ID, state, health, resource usage)
- `EventStream` -- real-time events (starts, stops, health changes, OOMs)
- `SchedulingDirective` -- from parent to Bun: start/stop/update workloads

**Bun <-> eBPF kernel maps:** Direct memory-mapped access via `libbpf` file descriptors. Map updates are atomic `bpf_map_update_elem()` calls. No serialisation protocol -- the map keys and values are C-compatible structs (`#[repr(C)]`).

**Bun <-> nftables:** Netlink socket via the `nft` library or direct `nftnl` bindings. Rules are expressed as nftables objects and committed atomically.

---

## 5. Operations

### 5.1 Container Lifecycle

**Start a container workload (full sequence):**

1. Receive `AppSpec` from WorkloadSupervisor (originally from the scheduler via the reporting tree).
2. Transition state to `Pending`.
3. **Prepare image:** Check Pickle local cache. If missing, fetch from Pickle peers (content-addressed, mTLS). If not in any Pickle node, pull from the external registry configured in `node.toml`. Transition to `Preparing`.
4. **Allocate port:** Request a host port from PortAllocator. The allocator picks a random available port from the configured range (default 10000-60000) and marks it as in-use.
5. **Create cgroup:** CgroupMgr creates a cgroup v2 hierarchy: `/sys/fs/cgroup/reliaburger/{namespace}/{app_name}/{instance}/`. Set `cpu.max` (from `cpu.limit`), `cpu.weight` (from `cpu.request`), `memory.max` (from `memory.limit`), `memory.high` (from `memory.request` -- enables graceful pressure before OOM). For GPU workloads, configure device allow-lists via the cgroup `devices` controller.
6. **Prepare network namespace:** Create a new network namespace. Configure the veth pair and port mapping (host port -> container internal port). Attach Onion eBPF programs to the namespace's sockets.
7. **Decrypt secrets:** For any `EnvValue::Encrypted` in the env map, decrypt using the cluster's age private key in memory. Plaintext is never written to disk.
8. **Prepare workload identity:** Generate a keypair, send CSR to the nearest council member, receive the signed X.509 certificate. Write cert, key, CA chain, and JWT to a tmpfs mount.
9. **Mount volumes and config files:** Bind-mount declared volumes from the host filesystem. Write `ConfigFileSpec` contents to temp files and bind-mount them read-only at the declared paths.
10. **Run init containers (Initialising):** For each init container in declaration order, create a container via containerd sharing the main container's network namespace and volumes. Wait for exit code 0. If any fails, transition to `Failed` and emit event.
11. **Create and start the main container (Starting):** Call `containerd::containers::create()` with the OCI spec (rootfs, namespaces, cgroups, mounts, env, seccomp). Call `containerd::tasks::start()`. Attach to stdout/stderr streams and pipe to Ketchup.
12. **Health wait (HealthWait):** After `initial_delay`, begin sending HTTP GET to `health` path via the container's network namespace IP and declared port. Continue at `interval` until `healthy_threshold` consecutive successes.
13. **Running:** Register the instance in the Onion service map (eBPF kernel map). The workload now receives traffic via DNS resolution and connect() rewriting. Emit a `WorkloadRunning` event to the reporting tree.
14. **Ongoing health checking:** HealthChecker continues probing at `interval`. If `unhealthy_threshold` consecutive failures: transition to `Unhealthy`, remove from Onion service map, emit event. If the workload recovers (`healthy_threshold` consecutive passes), transition back to `Running` and re-add to service map.
15. **Restart on persistent unhealthiness:** If the workload remains `Unhealthy` and the restart policy permits, transition to `Stopping`, send SIGTERM, wait `drain_timeout`, send SIGKILL if still alive, then loop back to step 2 with incremented `restart_count` and exponential backoff.

**Stop a container workload:**

1. Transition to `Stopping`. Remove from Onion service map.
2. Wrapper stops routing new connections. Existing connections drain for `drain_timeout`.
3. Send SIGTERM to the container's PID 1.
4. Wait up to `drain_timeout` for the container to exit.
5. If still running, send SIGKILL.
6. Transition to `Stopped`. Release the host port back to PortAllocator. Remove the cgroup. Clean up the network namespace. Emit event.

**Reconnect after Bun restart:**

1. On startup, Grill queries containerd for all containers with the `reliaburger.managed=true` label.
2. For each container, reconstruct the `WorkloadId` from container labels.
3. Re-attach to stdout/stderr streams (Ketchup log capture resumes).
4. Read the container's current state from containerd.
5. Resume health checking.
6. Re-populate the Onion service map from the discovered containers.
7. Compare the discovered state against the expected state from the reporting tree and reconcile (start missing workloads, stop unexpected ones).

### 5.2 Process Workload Lifecycle

**Start a process workload:**

1. Receive `AppSpec` from WorkloadSupervisor. Verify `WorkloadSource` is `Exec` or `Script`.
2. Transition to `Pending`.
3. **Validate binary (Preparing):** For `Exec`, check that the binary path appears in `node.toml`'s `[process_workloads] allowed_binaries`. If `allow_globs = false`, reject any pattern-matched entries. If the binary is not in the allowlist, transition to `Failed` with a clear error and emit an event. For `Script`, no binary validation is needed (the script runs via the declared shebang interpreter, which itself must be in a default-visible path like `/usr/bin`).
4. **Write inline script (Script only):** Write the script content to a temporary file in a Bun-managed temp directory (outside any workload's mount namespace). Mark it executable (`chmod 0755`). Compute and record the SHA-256 hash for the event log.
5. **Allocate port:** Same as container workloads -- request from PortAllocator if the workload declares a port.
6. **Create cgroup:** Identical to container workloads (step 5 of 5.1).
7. **Create namespaces:** Use `clone()` with `CLONE_NEWPID | CLONE_NEWNET | CLONE_NEWNS | CLONE_NEWUTS` to create a child process in isolated namespaces.
8. **Set up mount namespace:** In the child, create a restricted filesystem view:
   - **Default read-only mounts:** `/usr/bin`, `/usr/local/bin`, `/usr/lib`, `/usr/local/lib`, `/lib`, `/lib64`, `/etc/resolv.conf`, `/etc/hosts`, `/etc/ssl`
   - **Default read-write mounts:** A per-workload tmpfs at `/tmp`; the volume path if declared
   - **Blocked (not mounted):** `/etc/shadow`, `/etc/sudoers`, `/var/lib/reliaburger`, other workloads' paths, `/root`, `/home`, `/dev` (except granted GPU devices)
   - **Explicitly granted:** Each `allow_paths` entry is bind-mounted (read-only by default, read-write if `:rw` suffix)
9. **Set up network namespace:** Same as container workloads -- veth pair, port mapping, Onion eBPF attachment.
10. **Decrypt secrets and prepare identity:** Identical to container workloads (steps 7-8 of 5.1).
11. **Apply seccomp profile:** Load the default seccomp profile (blocks `mount`, `reboot`, `kexec_load`, `init_module`, etc.). Identical to the container seccomp profile.
12. **Drop privileges:** Switch to the `burger` user. Apply granted capabilities (if any) via `prctl(PR_CAP_AMBIENT)`.
13. **Exec the binary (Starting):** `execve()` the binary with args (for Exec), or the temp script file (for Script). Capture stdout/stderr and pipe to Ketchup.
14. **Health wait, running, health checking:** Identical to container workloads (steps 12-15 of 5.1).

**Stop a process workload:**

1. Identical to stopping a container, but signals are sent directly to the process PID rather than through containerd.
2. Cleanup includes: removing the cgroup, removing the network namespace, deleting the temp script file (if Script), releasing the host port.

### 5.3 Health Checking

Bun's HealthChecker runs as a dedicated async task that manages a priority queue of upcoming health checks across all workloads on the node.

**Algorithm:**

```
loop {
    next_check = priority_queue.peek() // earliest deadline
    sleep_until(next_check.deadline)

    instance = next_check.instance
    spec = next_check.health_spec

    // Probe via the workload's network namespace
    result = http_get(
        addr: instance.container_ip,
        port: instance.internal_port,
        path: spec.path,
        timeout: spec.timeout,
        namespace_fd: instance.network_namespace_fd,
    )

    health_result = match result {
        Ok(status) if status.is_success() => HealthStatus::Healthy,
        Ok(status) => HealthStatus::Unhealthy,
        Err(Timeout) => HealthStatus::Timeout,
        Err(ConnectionRefused) => HealthStatus::ConnectionRefused,
        Err(_) => HealthStatus::Unknown,
    }

    // Update consecutive counters
    match health_result {
        Healthy => {
            instance.consecutive_healthy += 1;
            instance.consecutive_unhealthy = 0;
        }
        _ => {
            instance.consecutive_unhealthy += 1;
            instance.consecutive_healthy = 0;
        }
    }

    // Evaluate state transitions
    if instance.state == HealthWait
       && instance.consecutive_healthy >= spec.healthy_threshold {
        transition(instance, Running)
        onion_service_map.add(instance)
    }

    if instance.state == Running
       && instance.consecutive_unhealthy >= spec.unhealthy_threshold {
        transition(instance, Unhealthy)
        onion_service_map.remove(instance)
    }

    if instance.state == Unhealthy
       && instance.consecutive_healthy >= spec.healthy_threshold {
        transition(instance, Running)
        onion_service_map.add(instance)
    }

    // Schedule the next check
    priority_queue.push(instance, now + spec.interval)

    // Emit result
    event_log.append(HealthCheckResult { ... })
}
```

**Health checks for workloads without a `health` field:** Bun uses process liveness as the health signal. If the main process exits, the workload is considered unhealthy and is restarted per the restart policy.

### 5.4 Cgroup Management

Bun uses cgroup v2 exclusively. The cgroup hierarchy is:

```
/sys/fs/cgroup/reliaburger/
  {namespace}/
    {app_name}/
      {instance}/
        cpu.max          = "{limit_us} {period_us}"
        cpu.weight       = {weight}    // derived from request
        memory.max       = {limit_bytes}
        memory.high      = {request_bytes}  // soft limit, triggers reclaim
        io.max           = {blkio_limits}   // for fault injection disk-io throttle
        cgroup.procs     = {pid}
```

**CPU accounting:**

- `cpu.max` is set from the `cpu.limit` field. For "500m" (500 millicores), this becomes `50000 100000` (50ms of CPU time per 100ms period).
- `cpu.weight` is derived from `cpu.request` relative to the node's total allocatable CPU. This provides proportional sharing when the node is contended.

**Memory accounting:**

- `memory.max` is the hard limit. When exceeded, the OOM killer activates. This maps to `memory.limit`.
- `memory.high` is set to `memory.request`. When exceeded, the kernel applies memory pressure (reclaims pages, slows allocations) without killing the process. This provides graceful degradation.
- OOM kills are detected by Bun via `memory.events` inotify. When an OOM event is detected, Bun logs it as a structured event with the full cgroup memory state, emits it to the reporting tree, and increments the restart counter.

**GPU device isolation:**

- Bun detects GPUs at startup via NVML and creates device allow-lists in the cgroup.
- `devices.allow` is written with the specific `/dev/nvidia{N}` device for the allocated GPU.
- NVML environment variables (`NVIDIA_VISIBLE_DEVICES`, `CUDA_VISIBLE_DEVICES`) are set in the workload's environment to the allocated device index.

### 5.5 Self-Upgrade Sequence

The self-upgrade mechanism replaces the Bun binary on every node in a rolling fashion. Bun is the orchestrator for its own upgrade.

**Node-level upgrade sequence (executed on each node):**

```
1. Receive upgrade directive from the leader (via reporting tree).
   Directive contains:
     - target_version: "v1.4.0"
     - binary_hash: [u8; 32]  (SHA-256)
     - embedded_signature: Vec<u8>  (Ed25519, from release key set)
     - external_signature: Vec<u8>  (Ed25519, from external key in node.toml)

2. Fetch the binary from Pickle (content-addressed by hash).
   The leader already stored it in Pickle after download and verification.

3. Verify integrity:
   a. SHA-256 of received binary must match binary_hash.
   b. Verify embedded_signature against the signing key set compiled
      into the CURRENT running binary.
   c. If node.toml has external_signing_key:
      verify external_signature against that key.
      (For network upgrades, external_signing_key is required.)
   d. If any verification fails: abort, report failure to leader, do NOT
      proceed.

4. Mark self as draining (stop accepting new work).
   Running containers continue serving.

5. Write the new binary to a staging path:
   /usr/local/bin/reliaburger-v1.4.0

6. Write the detached signature:
   /usr/local/bin/reliaburger-v1.4.0.sig

7. Update the symlink atomically:
   /usr/local/bin/reliaburger -> reliaburger-v1.4.0

8. Exec the new binary:
   execve("/usr/local/bin/reliaburger", ["reliaburger", "agent", ...])
   This replaces the current process in-place. The PID does not change.

9. New Bun starts up:
   a. Read node.toml.
   b. Reconnect to running containers via containerd (see 5.1 reconnect).
   c. Re-attach to stdout/stderr streams for log capture.
   d. Resume health checking.
   e. Re-populate the Onion service map.
   f. Rejoin the Mustard gossip mesh.
   g. Report healthy on the new version to the reporting tree.

10. If the new Bun fails to start (exits within 30s, or fails to rejoin
    gossip within 60s):
    a. The systemd unit (or equivalent) restarts Bun.
    b. On restart, Bun detects that the previous version is still present.
    c. Bun reverts the symlink to the previous version.
    d. Bun execs the previous version.
    e. Previous Bun starts, reconnects to containers, reports failure
       to the leader.
    f. Leader pauses the upgrade.
```

**Cluster-level upgrade orchestration (executed by the leader):**

```
1. Leader downloads the new binary from the release CDN (or receives
   it via --binary for air-gapped).
2. Leader verifies:
   a. SHA-256 checksum against release metadata.
   b. Embedded signature against the compiled-in key set.
   c. External signature against the external_signing_key in node.toml.
3. Leader stores the verified binary in Pickle.
4. Upgrade workers (configurable parallelism, default --parallel 1):
   For each batch of N worker nodes:
     a. Send upgrade directive to each node in the batch.
     b. Wait for all nodes in the batch to report healthy on the new version.
     c. If any node fails: pause the upgrade, alert the operator.
     d. If all nodes in the batch succeed: proceed to the next batch.
5. Upgrade council members (always one at a time):
   For each non-leader council member:
     a. Send upgrade directive.
     b. Wait for the node to report healthy.
     c. Verify Raft quorum is maintained.
     d. Proceed to the next council member.
6. Upgrade the leader (last):
   a. Transfer leadership to an already-upgraded council member.
   b. The new leader sends the upgrade directive to the former leader.
   c. Former leader upgrades itself.
   d. Former leader reports healthy. Upgrade complete.
```

**Rollback:**

```
$ relish upgrade rollback [version]
```

- Same rolling sequence as upgrade (workers -> council -> leader), but in reverse order.
- Previous binary is already on disk -- no download required.
- The leader rolls back last.
- Faster than the initial upgrade because there is no Pickle distribution step.

### 5.6 `node.toml` Parsing

Bun parses `/etc/reliaburger/node.toml` at startup. The parsing sequence:

1. Check for the file at `/etc/reliaburger/node.toml`. If absent, use all defaults (suitable for `relish init` on the first node).
2. Deserialize the TOML using `serde` with `#[serde(default)]` on all fields.
3. **Validate:**
   - `cluster.join` addresses must be valid `host:port` pairs.
   - `storage.*` paths must be absolute.
   - `resources.reserved_cpu` must be a valid resource string (e.g. "500m").
   - `resources.reserved_memory` must be a valid resource string (e.g. "512Mi").
   - `network.port_range` must have `start < end` and both in 1024-65535.
   - `process_workloads.allowed_binaries` entries must be absolute paths. If any contain glob characters (`*`, `?`, `[`) and `allow_globs` is false, fail with an error.
   - `upgrades.external_signing_key` must be a valid `ed25519:` prefixed key if present.
4. **Auto-detect missing values:**
   - `node.name`: read from `gethostname()`.
   - `network.advertise_address`: detect from the default route interface.
   - GPU labels (`gpu = "true"`, `gpu_model = "a100"`): detect via NVML if `resources.gpu_enabled` is true.
5. **Create storage directories** if they do not exist. Set ownership to the `reliaburger` system user.
6. **Hot reload:** Bun watches `node.toml` via `inotify`. On change, it re-parses and applies non-disruptive changes (labels, image config, log retention, metrics intervals). Changes to `storage.*` paths, `network.port_range`, or `cluster.join` require a Bun restart and are logged as warnings.

---

## 6. Configuration

All configuration is in `/etc/reliaburger/node.toml`. Every field has a default; the minimal configuration for joining a cluster is:

```toml
[cluster]
join = ["10.0.1.5:9443"]
```

### 6.1 Full Reference

| Section | Key | Default | Valid Range / Type | Description |
|---------|-----|---------|--------------------|-------------|
| `[node]` | `name` | hostname | string | Human-readable node name. Must be unique in the cluster. |
| `[node.labels]` | (any key) | (none) | string key-value pairs | Arbitrary labels for placement constraints. `zone`, `rack`, `storage`, `role` are common. |
| `[cluster]` | `join` | `[]` | list of `"host:port"` strings | Addresses of existing cluster members. Empty for first node. |
| `[storage]` | `data` | `/var/lib/reliaburger/data` | absolute path | Raft log, scheduling state. Small, critical. |
| `[storage]` | `images` | `/var/lib/reliaburger/images` | absolute path | OCI image layers (Pickle). Large, read-heavy. |
| `[storage]` | `logs` | `/var/lib/reliaburger/logs` | absolute path | Application logs (Ketchup). Append-heavy. |
| `[storage]` | `metrics` | `/var/lib/reliaburger/metrics` | absolute path | Time-series data (Mayo). Append-heavy, random reads. |
| `[storage]` | `volumes` | `/var/lib/reliaburger/volumes` | absolute path | Persistent app data. Must be reliable. |
| `[resources]` | `reserved_cpu` | `"500m"` | resource string (millicores) | CPU reserved for system + Bun. Not allocatable. |
| `[resources]` | `reserved_memory` | `"512Mi"` | resource string (bytes) | Memory reserved for system + Bun. Not allocatable. |
| `[resources]` | `reserved_disk` | `"10Gi"` | resource string (bytes) | Disk reserved for system. |
| `[resources]` | `gpu_enabled` | `true` | bool | Auto-detect GPUs via NVML at startup. |
| `[network]` | `advertise_address` | auto-detect | IP address | IP address this node advertises to the cluster. |
| `[network]` | `port_range` | `"10000-60000"` | `"start-end"`, both in 1024-65535 | Ephemeral port range for container port mapping. |
| `[images]` | `max_storage` | `"50Gi"` | resource string (bytes) | Maximum disk space for Pickle image layers. |
| `[images]` | `redundancy` | `2` | 1-10 | Number of peers to replicate pushed images to immediately. |
| `[images]` | `gc_retain_tags` | `10` | 1-1000 | Keep this many most-recent tags per repository. |
| `[images]` | `gc_retain_days` | `30` | 1-365 | Keep images accessed within this many days. |
| `[images]` | `pre_pull` | `true` | bool | Pre-pull images referenced by scheduled workloads. |
| `[images]` | `external_registries` | `[]` | list of registry objects | External OCI registries for pulling images not in Pickle. |
| `[logs]` | `retention_days` | `7` | 1-365 | Days to retain uncompressed logs. |
| `[logs]` | `compressed_retention_days` | `30` | 1-3650 | Days to retain compressed logs. |
| `[logs]` | `max_storage` | `"20Gi"` | resource string (bytes) | Maximum disk space for logs. |
| `[metrics]` | `collection_interval` | `"10s"` | duration, 1s-5m | How often to scrape workload metrics endpoints. |
| `[metrics]` | `retention_full` | `"24h"` | duration, 1h-7d | Retention for full-resolution metrics. |
| `[metrics]` | `retention_1m` | `"7d"` | duration, 1d-365d | Retention for 1-minute downsampled metrics. |
| `[metrics]` | `retention_1h` | `"90d"` | duration, 7d-3650d | Retention for 1-hour downsampled metrics. |
| `[metrics]` | `max_storage` | `"5Gi"` | resource string (bytes) | Maximum disk space for metrics. |
| `[ingress]` | `enabled` | `true` | bool | Whether this node serves external ingress traffic. |
| `[ingress]` | `http_port` | `80` | 1-65535 | HTTP listen port for Wrapper. |
| `[ingress]` | `https_port` | `443` | 1-65535 | HTTPS listen port for Wrapper. |
| `[ingress]` | `tls_acme_email` | (none) | email string | ACME account email for Let's Encrypt certificates. |
| `[process_workloads]` | `allowed_binaries` | `[]` | list of absolute path strings | Binaries permitted to run as process workloads. Empty = disabled. |
| `[process_workloads]` | `allow_globs` | `false` | bool | Allow glob patterns in `allowed_binaries`. |
| `[upgrades]` | `external_signing_key` | (none) | `"ed25519:..."` | External signing key for dual-signature verification. Required for network upgrades. |
| `[upgrades]` | `retain_versions` | `3` | 1-10 | Number of previous binary versions to keep on disk. |
| `[upgrades]` | `release_url` | `"https://releases.reliaburger.dev/metadata.json"` | URL | Release metadata endpoint for version checks. |

---

## 7. Failure Modes

### 7.1 Bun Process Crash

**What happens:** Bun exits unexpectedly (panic, SIGSEGV, OOM-killed).

**Detection:** systemd (or equivalent init system) detects the process exit. The Mustard gossip layer on peer nodes detects the heartbeat timeout (default 5 seconds).

**Impact:**

- Running containers are NOT affected. containerd/runc keeps them alive.
- eBPF programs persist in the kernel. Onion service discovery and firewall rules continue functioning for existing connections.
- No new containers can be started or stopped.
- Health checking pauses. Unhealthy instances are not removed from the service map until Bun restarts and re-evaluates.
- Log capture (Ketchup) stops. Logs are buffered in the container's stdout pipe (Linux pipe buffer is 64KB per default; larger with `F_SETPIPE_SZ`).
- Metrics collection (Mayo) stops.
- The Onion service map becomes stale (no updates for new workloads or health changes).

**Recovery:** systemd restarts Bun. On startup, Bun reconnects to running containers (see 5.1 reconnect), resumes all operations, and reconciles state with the cluster via the reporting tree.

### 7.2 Container Runtime (containerd) Crash

**What happens:** containerd exits unexpectedly.

**Detection:** Bun's Grill layer detects the broken gRPC connection to the containerd socket.

**Impact:**

- Running containers continue (runc is the actual runtime; containerd is the management layer).
- Bun cannot start, stop, or inspect containers until containerd restarts.
- Health checks that probe the workload's network endpoint still work (they use the network namespace directly, not containerd).

**Recovery:** systemd restarts containerd. containerd reconnects to running containers via shim processes. Bun's Grill layer re-establishes the gRPC connection and resumes operations.

### 7.3 Node Disk Full

**What happens:** A storage path runs out of space.

**Detection:** Bun monitors disk usage for each configured storage path. Alerts fire at 80% utilisation (warning) and 90% (critical). `relish wtf` reports disk pressure.

**Impact:** Depends on which path is full:

- `data` full: Raft log cannot append. Node cannot commit state changes. If this node is a council member, it falls behind.
- `images` full: Image pulls fail. New container starts that require image layers fail.
- `logs` full: Log writes fail. Bun drops log lines and increments a dropped-logs counter (visible in metrics and `relish wtf`).
- `metrics` full: Metric writes fail. Downsampled data is retained; full-resolution data is dropped.
- `volumes` full: Application writes fail with EDQUOT (Btrfs) or ENOSPC (loop mount).

**Recovery:** Bun triggers garbage collection on the affected path. For images, unreferenced layers are removed. For logs and metrics, retention policies are enforced aggressively. For volumes, the operator must increase the volume size or delete data.

### 7.4 Process Workload Binary Missing

**What happens:** The binary specified in `exec` does not exist at the declared path, or has been removed since the allowlist was configured.

**Detection:** Bun checks binary existence during the `Preparing` phase (step 3 of 5.2).

**Impact:** The workload transitions to `Failed`. An event is emitted with a clear error message: `"binary not found: /usr/local/bin/node_exporter"`.

**Recovery:** The operator installs the binary on the node and re-deploys, or removes the workload from the cluster configuration.

### 7.5 Health Check Endpoint Unreachable

**What happens:** The workload's health check endpoint does not respond, returns errors, or times out.

**Detection:** HealthChecker's `unhealthy_threshold` consecutive failure counter.

**Impact:** After `unhealthy_threshold` failures (default 3), the workload is marked `Unhealthy` and removed from the Onion service map. It stops receiving new traffic. If the restart policy permits, Bun restarts the workload.

**Recovery:** Automatic. If the workload recovers (returns healthy responses), it is re-added to the service map after `healthy_threshold` consecutive passes (default 1).

### 7.6 Self-Upgrade Failure

**What happens:** The new binary fails to start after the upgrade exec.

**Detection:** The new Bun exits within 30 seconds, or fails to rejoin gossip within 60 seconds.

**Impact:** This node is temporarily unable to accept new work. Running containers are unaffected (containerd manages them independently).

**Recovery:** Automatic rollback. Bun reverts the symlink to the previous version, execs the old binary, and reports the failure to the leader. The leader pauses the cluster-wide upgrade. The operator can fix the issue and resume, or roll back the entire cluster.

### 7.7 Gossip Partition

**What happens:** This node loses connectivity to the gossip mesh.

**Detection:** Mustard detects the absence of heartbeats from peers. The reporting tree parent detects the node as unreachable.

**Impact:**

- The node continues running its workloads normally.
- The cluster stops routing new traffic to this node's workloads (the service map on other nodes is updated by their respective Bun agents based on gossip/reporting tree state, not by this node).
- The node cannot receive new scheduling decisions.
- If the node was a council member, it may fall behind on Raft.

**Recovery:** When connectivity is restored, Mustard's gossip protocol converges within seconds. The reporting tree re-establishes. The node reconciles its state and resumes normal operation.

### 7.8 OOM Kill of a Workload

**What happens:** A workload exceeds its `memory.max` cgroup limit and the kernel's OOM killer terminates it.

**Detection:** Bun monitors `memory.events` via inotify on the cgroup. The `oom_kill` counter increment is detected within milliseconds.

**Impact:** The workload process is killed immediately. The workload transitions to `Stopped`, then restarts per the restart policy with exponential backoff.

**Recovery:** Automatic restart with backoff. The OOM event is logged with full memory state (current usage, limit, peak). `relish wtf` reports recent OOM kills.

---

## 8. Security Considerations

### 8.1 Attack Surface

| Surface | Risk | Mitigation |
|---------|------|------------|
| **Process workloads access host binaries** | Larger attack surface than containers (host filesystem access) | Binary allowlist in `node.toml` (deny-by-default), restricted mount namespace, seccomp profile, `burger` unprivileged user |
| **Self-upgrade replaces the Bun binary** | A compromised upgrade could replace the orchestrator | Dual-signature verification (embedded key set + external key), no auto-update, admin role required, binary verified twice (leader + each node) |
| **Secret decryption in Bun's memory** | Bun process memory contains plaintext secrets | Secrets are decrypted in memory only, never written to disk. `mlock()` the decryption buffer to prevent swapping. Zero the buffer after injection. |
| **eBPF programs run in kernel space** | A malicious eBPF program could compromise the kernel | eBPF programs are compiled into the Bun binary (not loaded from external files). The kernel's BPF verifier rejects unsafe programs. |
| **containerd socket access** | Access to the containerd socket allows arbitrary container operations | The socket is owned by root and the `reliaburger` group. Only Bun has access. It is NOT mounted into any workload's namespace. |
| **Smoker fault injection** | Faults could disrupt production | `admin` or `fault-injection` role required. Duration limits. No persistence across restarts. Blast radius protection (quorum, replica guards). |
| **`node.toml` contains external signing key** | Compromise of `node.toml` reveals the external key | File permissions 0600, owned by root. The external key is a public-key-like verification key, not a signing private key -- it verifies signatures, it cannot create them. |
| **Inline script execution** | Arbitrary code execution via git push | Lettuce enforces `require_signed_commits` for any config containing `script` fields. Scripts must be signed by a trusted key. |

### 8.2 Trust Boundaries

```
┌─────────────────────────────────────────────────────┐
│  Kernel space (fully trusted)                        │
│    eBPF programs (Onion, Smoker)                    │
│    cgroup controllers                                │
│    namespace isolation                               │
│    seccomp filters                                   │
│    nftables rules                                    │
└─────────────────────────────────────────────────────┘
                         │
                    syscall boundary
                         │
┌─────────────────────────────────────────────────────┐
│  Bun process (trusted, runs as reliaburger user)     │
│    Grill, ProcessManager, WorkloadSupervisor        │
│    Secret decryption, certificate management        │
│    eBPF map updates, cgroup management              │
│    Self-upgrade                                      │
└─────────────────────────────────────────────────────┘
                         │
              namespace + cgroup boundary
                         │
┌─────────────────────────────────────────────────────┐
│  Container workloads (untrusted)                     │
│    Isolated PID, network, mount, UTS, user namespace│
│    Seccomp profile applied                           │
│    Cgroup resource limits enforced                   │
│    No access to containerd socket, Bun state, or    │
│    other workloads' volumes/tmp                      │
└─────────────────────────────────────────────────────┘
                         │
              namespace + cgroup boundary (no user ns)
                         │
┌─────────────────────────────────────────────────────┐
│  Process workloads (partially untrusted)             │
│    Isolated PID, network, mount, UTS namespace      │
│    NO user namespace (intentional -- avoids UID      │
│    mapping issues with host binaries)                │
│    Same seccomp + cgroup as containers              │
│    Restricted filesystem view                        │
│    Runs as 'burger' unprivileged user               │
└─────────────────────────────────────────────────────┘
```

### 8.3 Restricted Capabilities

The following capabilities are explicitly NOT available to process workloads:

- `NET_ADMIN`: Cannot modify network configuration. Prevents interference with the cluster's networking layer.
- `NET_RAW`: Cannot craft arbitrary packets. Use `capture = true` for read-only packet capture scoped to the workload's own network namespace.

Available capabilities (require `admin` role, logged as security events):

- `SYS_PTRACE`, `DAC_OVERRIDE`, `CHOWN`, `SETUID`, `SETGID`

### 8.4 Seccomp Profile

The default seccomp profile blocks the following syscall families (identical for container and process workloads):

- `mount`, `umount2` -- prevents filesystem manipulation
- `reboot` -- prevents system reboot
- `kexec_load`, `kexec_file_load` -- prevents kernel replacement
- `init_module`, `finit_module`, `delete_module` -- prevents kernel module manipulation
- `pivot_root` -- prevents root filesystem changes
- `swapon`, `swapoff` -- prevents swap manipulation
- `sethostname`, `setdomainname` -- prevented even within UTS namespace for defense in depth
- `keyctl` -- prevents kernel keyring access

---

## 9. Performance

### 9.1 Design Targets

| Metric | Target | Rationale |
|--------|--------|-----------|
| Apps per node | 500 | Design goal from Section 2 |
| Per-app overhead (Bun tracking) | < 1ms | Sub-millisecond at 500 apps |
| Memory per app (Bun tracking) | ~1.2 MB | Measured: 312 MB at 500 apps vs 48 MB idle = ~528 KB tracking + workload overhead |
| Bun agent memory (idle) | ~48 MB | Baseline: Tokio runtime, eBPF management, gRPC channels, config |
| Bun agent memory (500 apps) | ~312 MB | Scales linearly with app count |
| eBPF service map entries | 500/node, 5M cluster-wide (10K nodes) | One entry per workload instance |
| eBPF map memory (kernel) | ~0.8 MB per node | Measured at ~5,800 entries |
| Health check latency (per probe) | < 5ms p99 (same node) | HTTP GET via network namespace; no proxy hop |
| Port allocation | O(1) | Bitmap-based allocator, random selection |
| Container start time (image cached) | < 1.2s to first healthy | Measured: containerd create + task start + health pass |
| Self-upgrade per node | ~40s | Drain + stage + exec + reconnect + health verify |
| Self-upgrade full cluster (12 nodes) | ~8 min | Sequential workers + sequential council + leader |

### 9.2 Scaling Considerations

**500 apps per node:** At 500 apps, Bun manages 500 cgroups, 500 network namespaces, 500 health check loops, and 500 entries in the Onion service map. The HealthChecker processes ~50 health checks per second (500 apps / 10s interval). Each check is a non-blocking HTTP GET that completes in microseconds to milliseconds. The Tokio runtime handles this without thread-per-check overhead.

**10,000 nodes:** Bun itself is not affected by cluster size. The reporting tree ensures that each Bun instance communicates only with its parent (a council member), not with all other nodes. The Onion service map on each node contains entries for all workloads in the cluster (~5M at max density), which fits comfortably in kernel memory (~80 bytes per entry = ~400 MB, well within typical server memory).

**Port allocation at density:** The default port range of 10000-60000 provides 50,000 ports. At 500 apps with rolling deploys (briefly 2x ports during transitions), this is 1,000 ports maximum = 2% utilisation. Port exhaustion is not a concern.

### 9.3 Memory Budget

```
Bun process:
  Tokio runtime + core:                 ~15 MB
  Grill (containerd client, state):     ~8 MB
  Onion (eBPF management, userspace):   ~5 MB
  Ketchup (log buffers):                ~5 MB
  Mayo (local metrics state):           ~5 MB
  Pickle (image index):                 ~3 MB
  Mustard (gossip state):               ~2 MB
  Wrapper (routing table, TLS):         ~3 MB
  Brioche (compiled UI assets):         ~2 MB
  ───────────────────────────────────────────
  Idle total:                           ~48 MB

  Per-workload overhead:
    State tracking + health:            ~0.5 KB
    Ketchup log buffer:                 ~4 KB
    Event ring buffer entry:            ~0.2 KB
    ────────────────────────────────────────
    Per-workload:                       ~5 KB
    500 workloads:                      ~2.5 MB

  Majority of the 312 MB at 500 apps is containerd client state,
  cached container metadata, and Ketchup log buffers for active
  streams.
```

---

## 10. Testing Strategy

### 10.1 Unit Tests

Unit tests cover individual components in isolation using mocked dependencies.

**Grill unit tests:**

- OCI spec generation from `AppSpec` (correct namespace flags, cgroup params, mount entries, env vars)
- Port allocation and release (no leaks, no duplicates, correct range enforcement)
- Cgroup path construction and parameter calculation (millicores to `cpu.max` conversion, memory limit encoding)
- Container state machine transitions (all valid transitions, rejection of invalid transitions)
- Reconnect logic (mock containerd returning containers with Reliaburger labels)

**ProcessManager unit tests:**

- Binary allowlist validation (exact match, glob match, glob rejection when `allow_globs = false`)
- Mount namespace construction (default visible paths, blocked paths, `allow_paths` with `:rw` suffix)
- Script temp file creation and hash computation
- Isolation config derivation from `AppSpec`

**HealthChecker unit tests:**

- Priority queue scheduling (correct ordering by deadline)
- Consecutive counter logic (healthy threshold, unhealthy threshold, counter reset)
- State transition decisions (HealthWait -> Running, Running -> Unhealthy, Unhealthy -> Running)
- Timeout handling (probe timeout vs health_timeout)

**NodeConfig unit tests:**

- TOML parsing with all defaults
- TOML parsing with all fields specified
- Validation errors (invalid port range, glob without `allow_globs`, non-absolute paths)
- Auto-detection fallbacks (hostname, advertise address)

**UpgradeManager unit tests:**

- Signature verification (valid signature, invalid signature, missing external key)
- Symlink management (create, update, rollback)
- Version retention (keep N versions, garbage collect older)

### 10.2 Integration Tests

Integration tests run against a real cluster using `relish test`. Bun-specific tests:

```
Process Workloads
  - process app runs, gets health checked, appears in service map
  - inline script job runs and completes
  - process workload has correct namespace/cgroup isolation
  - process workload binary not in allowlist is rejected
  - process workload cannot see /var/lib/reliaburger
  - process workload cannot see other workloads' volumes
  - allow_paths mounts are accessible with correct permissions

Health Checks
  - app returns 200, verify marked healthy
  - app returns 500 after 30s, verify marked unhealthy + rescheduled
  - app hangs (no response), verify timeout detection
  - health check recovery re-adds to service map

Container Lifecycle
  - deploy app with 3 replicas, verify placement
  - rolling deploy with zero downtime (continuous health probe)
  - deploy broken image, verify auto-rollback
  - volume write survives container restart
  - init container failure prevents main container start

Self-Upgrade (requires dedicated test cluster)
  - upgrade single node, verify containers survive
  - rollback single node, verify revert
  - full rolling upgrade across cluster
  - upgrade failure triggers automatic rollback on affected node
```

**Test apps:** A lightweight HTTP server (~200 lines of Rust) is compiled into the Bun binary. It supports configurable health check behaviour: return 200, return 500 after N seconds, hang indefinitely, or allocate memory until OOM. This enables integration tests to exercise failure modes without external dependencies.

### 10.3 Chaos Tests

Chaos tests use Smoker fault injection to verify resilience. Bun-relevant chaos tests:

```
Node Failure
  - kill node, verify replicas rescheduled to surviving nodes
  - drain node, verify zero-downtime migration
  - kill node running volume app, verify alert fires

Resource Exhaustion
  - OOM kill app instance, verify restart + health recovery
  - CPU stress to 95%, verify app continues serving (degraded)
  - fill disk to 90%, verify alert fires + gc triggers

Process Faults
  - SIGKILL a workload, verify restart and health recovery
  - SIGSTOP a workload, verify health checks fail and trigger restart
  - SIGCONT a frozen workload, verify health recovery

Bun Restart
  - kill Bun process, verify containers continue running
  - kill Bun process, verify Bun reconnects on restart
  - kill Bun during a rolling deploy, verify deploy resumes after restart
```

### 10.4 Benchmark Coverage

`relish bench` measures Bun-specific metrics:

- **Time to first healthy instance:** Container pull + create + start + health pass. Target: < 1.2s (image cached).
- **Memory overhead per app:** Bun memory at N apps minus Bun memory idle, divided by N. Target: ~1.2 MB.
- **Max concurrent apps sustained:** Ramp app count until Bun becomes unstable or per-app overhead exceeds 1ms. Target: 500.

---

## 11. Prior Art

### 11.1 Kubernetes kubelet

**What it does:** The kubelet is the per-node agent in Kubernetes. It watches the API server for Pod specs assigned to its node, manages container lifecycle via the Container Runtime Interface (CRI), performs health checks (liveness, readiness, startup probes), and reports node/pod status.

**Docs:** https://kubernetes.io/docs/reference/command-line-tools-reference/kubelet/ and https://github.com/kubernetes/kubernetes/tree/master/pkg/kubelet

**What we borrow:**

- The three-probe health check model (liveness/readiness/startup) inspired our health check state machine, though we simplify it to a single `health` endpoint with `initial_delay` + threshold-based state transitions. Kubernetes's three separate probe types with different consequences (liveness kills, readiness removes from service, startup gates both) adds operational complexity that most users get wrong. Our single probe covers readiness semantics (remove from service map when unhealthy, re-add when healthy) and liveness semantics (restart after persistent unhealthiness).
- The concept of node-level resource reservations (`reserved_cpu`, `reserved_memory`) for system overhead.
- The CRI abstraction (our Grill serves the same purpose).

**What we do differently:**

- kubelet communicates with the API server directly; Bun uses a hierarchical reporting tree to avoid all-to-all communication.
- kubelet manages Pods (groups of containers with shared namespaces); Bun manages individual workloads (one process per isolation boundary). The Pod abstraction adds complexity that is rarely needed.
- kubelet has no concept of process workloads. Running host binaries under orchestrator control requires workarounds (hostPath volumes, privileged containers, or DaemonSets with host networking).
- kubelet does not manage its own upgrades. Cluster upgrades require external tooling (kubeadm, kops, EKS/GKE managed control plane).

### 11.2 HashiCorp Nomad Client

**What it does:** The Nomad client agent runs on every node, receives task allocations from the server, and manages task lifecycle via task drivers (Docker, exec, raw_exec, java, etc.).

**Docs:** https://developer.hashicorp.com/nomad/docs/concepts/architecture and https://developer.hashicorp.com/nomad/docs/drivers

**What we borrow:**

- The `exec` and `raw_exec` task drivers are the direct inspiration for our process workloads. Nomad proved that non-container workloads managed by an orchestrator is a valuable pattern.
- The concept of task drivers as pluggable runtime backends (our Grill + ProcessManager serve the same role, but compiled in rather than pluggable).

**What we do differently:**

- Nomad's `raw_exec` runs with no isolation by default (no namespaces, no cgroups). Our process workloads always get full isolation (PID, network, mount, UTS namespaces + cgroups + seccomp). There is no "raw" mode.
- Nomad's client does not include service discovery, metrics, or log collection. These are separate systems (Consul, Prometheus, external log aggregator). Bun includes all of them.
- Nomad does not include a built-in image registry or eBPF service mesh. These are external concerns.

### 11.3 Docker / Moby

**What it does:** Docker provides a container runtime and management daemon (dockerd) that handles image pull, container creation, lifecycle management, and networking.

**Docs:** https://docs.docker.com/engine/ and https://github.com/moby/moby

**What we borrow:**

- The UX model of declarative container specs with sensible defaults.
- The concept of port mapping (container port -> host port) for networking without overlays.

**What we do differently:**

- Docker is a general-purpose container runtime; Grill is purpose-built for Reliaburger's specific needs (eBPF integration, Onion service map updates, Ketchup log capture, Mayo metrics).
- Docker uses its own runtime; Grill uses containerd (which Docker also uses internally, but we skip the Docker daemon layer).
- Docker has no concept of cluster scheduling, health-check-aware service discovery, or process workloads.

### 11.4 containerd

**What it does:** containerd is a container runtime daemon that manages the complete container lifecycle: image transfer, container execution via runc, storage, and network namespace setup.

**Docs:** https://containerd.io/docs/ and https://github.com/containerd/containerd

**What we borrow:**

- containerd is our actual container runtime. Grill is an abstraction layer on top of containerd's gRPC API.
- containerd's shim model (shim processes that outlive the daemon) is what enables containers to survive Bun restarts.
- Content-addressed storage for image layers (Pickle extends this pattern to cluster-wide distribution).

**What we do differently:**

- We use containerd as a library/service, not as a user-facing tool. Operators never interact with containerd directly.
- We do not use containerd's built-in CRI plugin (which is designed for Kubernetes). We use the lower-level containerd API directly via Grill.

### 11.5 systemd

**What it does:** systemd is the init system and service manager on most Linux distributions. It manages service lifecycle, cgroups, and logging (journald).

**Docs:** https://www.freedesktop.org/wiki/Software/systemd/ and https://systemd.io/

**Relevance:**

- Process workloads that Bun manages are similar in spirit to systemd units. The difference is that Bun integrates them into the cluster's scheduling, service discovery, health checking, and metrics pipeline -- things that systemd cannot provide.
- Bun itself runs as a systemd unit for process supervision (restart on crash).
- systemd's cgroup management model (one cgroup per unit) inspired our one-cgroup-per-workload model, though we manage cgroups directly rather than delegating to systemd.

---

## 12. Libraries & Dependencies

### 12.1 Rust Crates

| Crate | Purpose | Version | License | Maturity | Notes |
|-------|---------|---------|---------|----------|-------|
| `containerd-client` | gRPC client for containerd API | latest | Apache-2.0 | Stable | Official containerd Rust client. Provides typed bindings for container and task management. Alternative: raw tonic gRPC with protobuf definitions, but the official client saves significant effort. |
| `tokio` | Async runtime | 1.x | MIT | Mature, production-grade | The foundation for all async operations. Multi-threaded runtime with work-stealing scheduler. Pin Bun's critical tasks (health checker, reporting tree) to dedicated threads at high density. |
| `tonic` | gRPC framework | 0.12+ | MIT | Mature | Used for inter-node communication (reporting tree, API forwarding, CSR flow). Also used internally by `containerd-client`. |
| `nix` | Safe Rust bindings for Unix APIs | 0.29+ | MIT | Mature | `clone()`, `unshare()`, `mount()`, `pivot_root()`, `setns()`, `prctl()`, signal handling. Essential for process workload namespace setup. |
| `libbpf-rs` | eBPF program loading and map management | 0.24+ | BSD-2-Clause | Active development | Loads compiled eBPF programs, manages BPF maps, handles kernel version differences. Used by Onion, Smoker, and Bun's cgroup monitoring. Alternative: `aya` (pure Rust eBPF). `libbpf-rs` chosen because it wraps the canonical C `libbpf` library, providing maximum kernel compatibility and access to CO-RE (Compile Once, Run Everywhere). `aya` is compelling but younger and may have edge cases with older kernels. |
| `toml` + `serde` | TOML parsing and serialisation | latest | MIT/Apache-2.0 | Mature | For `node.toml` parsing and all TOML-based configuration. `serde` provides `#[serde(default)]` for optional fields with defaults. |
| `age` | age encryption/decryption | 0.10+ | MIT/Apache-2.0 | Stable | For decrypting `ENC[AGE:...]` secrets. The `age` crate is the canonical Rust implementation of the age encryption format. |
| `ed25519-dalek` | Ed25519 signature verification | 2.x | BSD-3-Clause | Mature | For verifying binary signatures during self-upgrade. Both embedded key set verification and external key verification. |
| `sha2` | SHA-256 hashing | 0.10+ | MIT/Apache-2.0 | Mature | For binary integrity verification during self-upgrade. Also used for content addressing in Pickle. |
| `hyper` | HTTP client/server | 1.x | MIT | Mature | For health check probes (HTTP GET to workload endpoints), API server, and Wrapper ingress proxy. Used via Tokio. |
| `rustls` | TLS implementation | 0.23+ | MIT/Apache-2.0/ISC | Mature | For mTLS (inter-node, workload identity). Pure Rust, no OpenSSL dependency. Used by `tonic` for gRPC TLS and by `hyper` for HTTPS. |
| `notify` | Filesystem event watcher | 6.x | Artistic-2.0/MIT | Mature | For watching `node.toml` changes (hot reload) and cgroup `memory.events` (OOM detection). Uses `inotify` on Linux. |
| `nftnl` / `mnl` | nftables management | latest | GPL-2.0 / LGPL-2.1 | Stable | For managing perimeter firewall rules. `nftnl` provides high-level nftables object construction; `mnl` provides Netlink communication. Note: GPL-2.0 license on `nftnl` -- evaluate whether this is acceptable or whether we need to shell out to the `nft` CLI instead. |
| `nvml-wrapper` | NVIDIA Management Library bindings | 0.10+ | MIT/Apache-2.0 | Stable | For GPU detection at startup. Optional runtime dependency -- Bun functions without it (GPU features disabled). |
| `cron` | Cron expression parsing | 0.12+ | MIT/Apache-2.0 | Stable | For parsing Job schedule expressions. |
| `rand` | Random number generation | 0.8+ | MIT/Apache-2.0 | Mature | For port allocation (random selection from available pool) and fault injection probability calculations. |

### 12.2 Evaluation Notes

**`libbpf-rs` vs `aya`:** Both provide eBPF support in Rust. `libbpf-rs` wraps the C `libbpf` library and provides CO-RE support, meaning eBPF programs compiled once run on different kernel versions without recompilation. `aya` is a pure Rust implementation that avoids the C dependency but is younger and has less battle-tested kernel compatibility. For a system that must run on kernel 5.7+ across diverse distributions, `libbpf-rs`'s maturity and CO-RE support are the safer choice. Revisit if `aya` matures to equivalent kernel coverage.

**`nftnl` GPL concern:** The `nftnl` crate has a GPL-2.0 license. If this is incompatible with the project's licensing, the alternative is to invoke the `nft` CLI as a subprocess for nftables management. This is slightly less performant (process spawn per rule change) but avoids the license issue. Atomic rule application is still possible via `nft -f <file>`.

**`containerd-client` vs raw gRPC:** Using the official `containerd-client` crate saves effort and ensures API compatibility. The trade-off is a dependency on the containerd project's release cadence. If the crate lags behind containerd releases, we can fall back to raw `tonic` gRPC with protobuf definitions generated from containerd's `.proto` files.

---

## 13. Open Questions

1. **Cgroup delegation model:** Should Bun create cgroups directly under `/sys/fs/cgroup/reliaburger/`, or should it use systemd's `Delegate=yes` mechanism to receive a delegated cgroup subtree? Direct creation is simpler but may conflict with systemd's cgroup management on some distributions. Delegated cgroups are the "correct" approach on systemd systems but add complexity.

2. **Process workload user namespace:** The whitepaper explicitly excludes user namespaces from process workloads to avoid UID/GID mapping issues with host binaries. Should there be an opt-in `user_namespace = true` for process workloads that can tolerate it? This would provide stronger isolation at the cost of potential binary compatibility issues.

3. **HealthChecker concurrency model:** At 500 apps with 10-second intervals, the HealthChecker issues ~50 probes/second. This is easily handled by a single async task. But if health check timeouts are long (e.g., 30 seconds for a slow backend), many probes may be in-flight simultaneously. Should health checks have a per-node concurrency limit? Or should we rely on Tokio's task scheduling and the timeout mechanism?

4. **containerd version pinning:** Should Bun pin to a specific containerd version (and potentially bundle it), or should it support a range of containerd versions? Bundling ensures compatibility but increases the binary size and maintenance burden. Supporting a range requires version detection and conditional code paths.

5. **Process workload restart backoff ceiling:** What should the maximum restart backoff be for a persistently failing process workload? Container orchestrators typically cap at 5 minutes. Should process workloads have a different ceiling, given that they are often system-level daemons that the operator expects to keep trying?

6. **Volume snapshot atomicity on ext4/xfs:** On Btrfs, snapshots are instantaneous and atomic (CoW). On ext4/xfs, the fallback is a directory copy, which is neither instantaneous nor atomic. Should we require Btrfs for volume snapshots, or should we implement a freeze/copy/thaw sequence for ext4/xfs (using `fsfreeze` or `FIFREEZE` ioctl)?

7. **`node.toml` hot reload scope:** Which configuration changes should be hot-reloadable vs require a Bun restart? Currently proposed: labels, image config, log retention, and metrics intervals are hot-reloadable. Storage paths, port range, and cluster join addresses require restart. Should `process_workloads.allowed_binaries` be hot-reloadable (allowing operators to add/remove permitted binaries without restarting Bun)?

8. **GPU multi-tenancy:** The current model is whole-device GPU allocation (`gpu = 1` allocates one entire GPU). Should Bun support GPU time-slicing or MIG (Multi-Instance GPU) partitioning for NVIDIA A100/H100? This would allow multiple workloads to share a GPU, improving utilisation but adding significant complexity to the Grill layer.

9. **Self-upgrade rollback detection timeout:** Currently, if the new Bun exits within 30 seconds or fails to rejoin gossip within 60 seconds, it auto-reverts. Are these timeouts appropriate for all environments? Should they be configurable in `node.toml`?

10. **nftables vs pure eBPF for perimeter rules:** The current design uses nftables for perimeter enforcement (cluster boundary, management access, egress allowlists) and eBPF for inter-app rules. Could perimeter rules also move to eBPF, eliminating the nftables dependency entirely? This would simplify the dependency tree but may lose nftables' mature logging and accounting features.
