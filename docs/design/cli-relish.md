# Relish: CLI & TUI Design Document

## 1. Overview

Relish is the CLI and interactive terminal UI for Reliaburger. It's a single Rust binary that replaces five separate tools from the Kubernetes ecosystem: `kubectl` (cluster management), `k9s` (interactive TUI), `stern` (multi-instance log streaming), `kubectl-debug` (debug containers), and `terraform plan` (change previewing). Every debugging, diagnostic, configuration, and operational capability is compiled into the binary. There's nothing to install, no plugins to manage, no separate monitoring stack to query, and no shell scripts to maintain.

Relish operates in two modes:

- **CLI mode:** When invoked with a subcommand (`relish status`, `relish apply production/ --dry-run`, `relish wtf`), it executes the command, prints output, and exits. Suitable for scripting, CI pipelines, and quick one-off operations.
- **TUI mode (Phase 13):** When invoked with no arguments (`relish`), it launches a full-screen interactive terminal UI similar to k9s or htop, intended as the primary operational interface for day-to-day cluster management. It provides real-time views of apps, nodes, jobs, events, logs, and routes with keyboard-driven navigation. This is the Phase 13 deliverable (`src/relish/tui/`); the earlier phases ship the CLI mode above, and bare `relish` returned a usage error until Phase 13 wired the TUI in.

Both modes use the same underlying API client and output formatting. Anything visible in the TUI can also be retrieved via CLI commands, and anything scriptable via the CLI is navigable in the TUI.

### Design Principles

1. **Zero-install debugging.** Every diagnostic tool is built in. An on-call engineer with the Relish binary and a valid token can diagnose any cluster issue without installing additional software.
2. **Plan before apply.** Borrowing from Terraform, `relish apply <path> --dry-run` shows exactly what will change before anything is applied. No more "apply and hope."
3. **Correlation over enumeration.** Commands like `relish wtf` don't just list problems -- they correlate events, link crashloops to recent deploys, and suggest specific remediation.
4. **Scriptable by default.** Every command supports `--output json` for machine consumption, returns meaningful exit codes, and accepts filter flags for CI integration.
5. **Single binary, single version.** Relish is compiled into the same binary as Bun (the node agent). There's no version skew between client and server. The binary self-identifies its version and the cluster API rejects incompatible clients with a clear upgrade message.

---

## 2. Dependencies

Relish is a pure client-side binary. It doesn't run any server processes, doesn't maintain local state beyond a configuration file, and doesn't require network access except to reach the cluster API. All data comes from the cluster.

### Cluster API (all remote operations)

Every Relish command that interacts with the cluster communicates through the Reliaburger cluster API, exposed on port 9117 (mTLS) on every node. The API follows the request routing described in the whitepaper:

- **Read-only requests** (status, inspect, logs, top, resolve, routes, history, wtf) are served by any council member from its local Raft state replica. The receiving node forwards to the nearest council member if it isn't one itself.
- **Write requests** (apply, deploy, rollback, secret encrypt, snapshot create, fault inject) are forwarded to the leader, which commits via Raft before responding.

Relish can target any node in the cluster. It doesn't need to know which node is the leader.

### Bun Agent (exec)

The `relish exec` command requires the Bun agent running on the target node. Bun handles:

- Container namespace entry for `exec` (entering the container's PID, mount, and network namespaces) and running the command there.
- Streaming the exec session's stdin/stdout/stderr over the API connection.

The `exec --debug`, `exec --privileged` and `exec --node` variants (debug containers, firewall bypass, host-level execution) are **not yet implemented**.

### Onion (resolve, trace)

The `relish resolve` and `relish trace` commands inspect Onion's userspace and,
where attached, kernel state:

- `resolve` queries the userspace service map to show virtual IPs, real backends, health status, and node placement for a given service name.
- `trace` runs a real DNS query and TCP connect inside the source workload, then reads the userspace service map and any attached eBPF backend and cgroup-firewall maps.

Relish first finds the node hosting the source instance. That Bun agent owns
the network locality and live map handles needed to make the observations.

### Mayo (metrics queries)

The TUI resource-usage displays query the Mayo time-series database for CPU, memory, GPU, disk, and network metrics. Mayo runs locally on each node. Note that the CLI `relish top` and `relish inspect` do **not** show Mayo metrics — they print workload state, PID and restart counts from the local agent.

### Ketchup (log and history queries)

The `relish logs` and `relish history` commands query the Ketchup log store. Ketchup stores application logs on each node with configurable retention. Event *streaming* is available in the TUI and web dashboard, not as a `relish events` CLI command.

### Wrapper (route queries)

The `relish routes` command queries the Wrapper ingress proxy for the current routing table. (The command is `routes`, with no hostname argument.)

### Sesame (security)

`relish init`, `relish join`, `relish join-token`, `relish token`, `relish secret` and `relish sign` drive the Sesame security subsystem (PKI, identities, tokens, secret encryption, image signing). There is no `relish identity` or `relish ca` command; workload-identity and CA-management subcommands are **not implemented**.

---

## 3. Architecture

### High-Level Component Diagram

```
+------------------------------------------------------------------+
|  Relish Binary                                                    |
|                                                                   |
|  +------------------+    +------------------+                     |
|  |   CLI Dispatch   |    |   TUI Framework  |                     |
|  |                  |    |                  |                     |
|  |  clap argument   |    |  ratatui render  |                     |
|  |  parsing, sub-   |    |  loop, crossterm |                     |
|  |  command routing  |    |  event handling  |                     |
|  +--------+---------+    +--------+---------+                     |
|           |                       |                               |
|           v                       v                               |
|  +--------------------------------------------+                  |
|  |          Command Executors                  |                  |
|  |                                             |                  |
|  |  StatusCmd, ApplyCmd, DeployCmd, LogsCmd,   |                  |
|  |  TraceCmd, InspectCmd, WtfCmd, DiffCmd,     |                  |
|  |  ExecCmd, TopCmd, HistoryCmd, ...           |                  |
|  +---------------------+----------------------+                  |
|                         |                                         |
|           +-------------+-------------+                           |
|           v                           v                           |
|  +------------------+       +------------------+                  |
|  |   API Client     |       | Output Formatter |                  |
|  |                  |       |                  |                  |
|  |  reqwest HTTP/2  |       |  human-readable  |                  |
|  |  mTLS, token     |       |  JSON, table,    |                  |
|  |  auth, WebSocket |       |  TOML             |                  |
|  |  streaming       |       |                  |                  |
|  +--------+---------+       +------------------+                  |
|           |                                                       |
+-----------|-------------------------------------------------------+
            |
            v
    +------------------+
    |  Cluster API     |
    |  (any node:9117) |
    +------------------+
```

### CLI Command Dispatch

Relish uses `clap` (derive API) for argument parsing. The top-level binary dispatches to one of three paths:

1. **No arguments:** Launch the TUI event loop.
2. **Subcommand provided:** Parse subcommand arguments, construct the appropriate command executor, run it, format output, and exit.
3. **`--help`, `--version`:** Print help text or version info and exit.

Each command executor is a standalone async function that:

- Validates arguments locally (e.g., `relish lint` validates TOML syntax without contacting the cluster).
- Calls the API client for remote data.
- Formats and prints output.
- Returns an exit code (0 for success, 1 for errors, 2 for warnings-only in lint/wtf).

### TUI Framework

The TUI is built on `ratatui` (terminal rendering) and `crossterm` (terminal event handling). It runs an async event loop with three input sources:

1. **Terminal events:** Keyboard input, terminal resize. Polled via `crossterm::event::EventStream`.
2. **API data:** Periodic polling of cluster state (apps, nodes, events, metrics). Each view has its own refresh interval.
3. **Streaming data:** WebSocket connections for live logs, events, and exec sessions. Managed by `tokio` tasks that push updates into a channel.

The TUI maintains a view stack. The top-level view shows the dashboard (apps, nodes, recent events, alerts). Pressing a navigation key pushes a new view onto the stack. Pressing `Esc` or `q` pops back to the previous view. Pressing `q` from the top-level view exits.

**Rendering pipeline:**
```
Input Event
    |
    v
State Update (TuiState mutation)
    |
    v
Layout Calculation (ratatui::Layout)
    |
    v
Widget Rendering (ratatui::Frame::render_widget)
    |
    v
Terminal Flush (crossterm::execute)
```

The render loop targets 10 FPS for smooth scrolling and navigation. Data refresh is decoupled from rendering -- API polls happen on independent timers (2s for `top`-style metrics, 5s for app/node lists, real-time for streaming logs/events).

### API Client

The API client is a thin wrapper around `reqwest` configured with:

- **HTTP or HTTPS** to the Bun agent. With `--ca-cert` (or `RELIABURGER_CA_CERT`) the client trusts the cluster root CA and uses HTTPS; otherwise it talks plaintext HTTP to the local agent. There is no client-certificate file on disk.
- **Token authentication** via a `Bearer <token>` header from `--token` or `RELIABURGER_TOKEN`.
- **Fixed local endpoint** by default (`http[s]://127.0.0.1:9117`), overridable with `--endpoint` / `RELIABURGER_ENDPOINT`. There is no automatic multi-node endpoint discovery.
- **WebSocket upgrade** for streaming endpoints (logs, exec).
- **Retry with backoff** for transient failures (connection refused, 503). Maximum 3 retries with 1s/2s/4s backoff.
- **Request timeout:** 30s default, configurable per-command. Streaming connections have no timeout.

### Output Formatting

Every command supports three output modes via the `--output` flag:

| Mode | Flag | Description |
|------|------|-------------|
| Human | `--output human` (default) | Coloured, aligned terminal output with Unicode symbols |
| JSON | `--output json` | Machine-readable JSON, one object per line for streaming commands |
| YAML | `--output yaml` | YAML serialisation via `serde_yaml`, useful for config round-tripping |

Human-readable output uses ANSI colours (auto-detected, disabled when piped). Status indicators use Unicode: checkmarks, crosses, warning triangles, and bullets. Progress bars use `indicatif` for long-running operations (apply, upgrade, bench).

---

## 4. Data Structures

All data structures are Rust structs with `serde::Serialize` and `serde::Deserialize` for JSON serialisation. The TUI state structs also derive `Clone` for snapshot-based rendering.

### CLI Configuration

There is **no on-disk CLI config file** (no `~/.config/relish/config.toml`) and
**no OS keychain**. The CLI is configured entirely by global flags and
environment variables (see §6 Configuration). The only persisted state is what
`relish init` / `relish join` write — PKI, identity and config for the *agent*,
not for the CLI. The one real output-format type is:

```rust
/// Selected by the global `--output` flag (default `human`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OutputFormat {
    Human,
    Json,
    Yaml,
}
```

Connection settings come from these flags, each with an environment fallback:

| Flag | Env fallback | Meaning |
|------|--------------|---------|
| `--endpoint <url>` | `RELIABURGER_ENDPOINT` | Bun API base URL (default `http[s]://127.0.0.1:9117`) |
| `--token <token>` | `RELIABURGER_TOKEN` | API bearer token |
| `--ca-cert <path>` | `RELIABURGER_CA_CERT` | Cluster root CA PEM; switches the local default to HTTPS |
| `--output <fmt>` | — | `human`, `json` or `yaml` |

### TUI State

```rust
/// Top-level TUI application state.
#[derive(Debug, Clone)]
pub struct TuiState {
    /// Current view stack. Last element is the active view.
    pub view_stack: Vec<ViewKind>,

    /// Cluster-wide summary data.
    pub cluster: ClusterSummary,

    /// Per-view state.
    pub apps_view: AppsViewState,
    pub nodes_view: NodesViewState,
    pub jobs_view: JobsViewState,
    pub events_view: EventsViewState,
    pub logs_view: LogsViewState,
    pub routes_view: RoutesViewState,
    pub search_view: SearchViewState,

    /// Active alerts.
    pub alerts: Vec<Alert>,

    /// Status bar message (errors, confirmations).
    pub status_message: Option<(String, StatusLevel)>,

    /// Whether data is currently being fetched.
    pub loading: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ViewKind {
    Dashboard,
    Apps,
    AppDetail(String),        // app name
    Nodes,
    NodeDetail(String),       // node name
    Jobs,
    JobDetail(String),        // job name
    Events,
    Logs(Option<String>),     // optional app name filter
    Routes,
    RouteDetail(String),      // hostname
    Search,
    Help,
}
```

### App View

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppView {
    pub name: String,
    pub namespace: String,
    pub image: String,
    pub replicas_ready: u32,
    pub replicas_desired: u32,
    pub status: AppStatus,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub memory_display: String,     // e.g., "412Mi"
    pub gpu_used: u32,
    pub gpu_total: u32,
    pub restarts_recent: u32,       // restarts in last 5 minutes
    pub uptime_seconds: u64,
    pub last_deploy: Option<DeployInfo>,
    pub instances: Vec<InstanceView>,
    pub placement: PlacementInfo,
    pub ingress: Vec<IngressEntry>,
    pub identity: IdentityInfo,
    pub env_vars: Vec<EnvVar>,
    pub alerts: Vec<Alert>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AppStatus {
    Healthy,
    Degraded,
    Crashloop,
    Deploying,
    Scaling,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceView {
    pub name: String,           // e.g., "web-1"
    pub node: String,           // e.g., "node-01"
    pub port: u16,              // host port
    pub status: InstanceStatus,
    pub cpu_millicores: u32,
    pub memory_bytes: u64,
    pub uptime_seconds: u64,
}
```

### Node View

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeView {
    pub name: String,
    pub role: NodeRole,             // Council, Worker
    pub is_leader: bool,
    pub apps_count: u32,
    pub cpu_percent: f64,
    pub memory_percent: f64,
    pub disk_percent: f64,
    pub gpu_used: u32,
    pub gpu_total: u32,
    pub labels: HashMap<String, String>,
    pub running_apps: Vec<String>,
    pub disk_mounts: Vec<DiskMount>,
    pub ebpf_service_map_entries: u32,
    pub pickle_cache_bytes: u64,
    pub gossip_peers: Vec<String>,
    pub council_status: Option<CouncilMemberStatus>,
    pub uptime_seconds: u64,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskMount {
    pub path: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub filesystem: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeRole {
    Council,
    Worker,
}
```

### Event Stream

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub timestamp: DateTime<Utc>,
    pub event_type: EventType,
    pub severity: Severity,
    pub app: Option<String>,
    pub node: Option<String>,
    pub instance: Option<String>,
    pub message: String,
    pub actor: Option<Actor>,       // who caused this event
    pub details: serde_json::Value, // event-type-specific payload
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventType {
    Deploy,
    Scale,
    Health,
    Alert,
    Restart,
    OomKill,
    NodeJoin,
    NodeLeave,
    LeaderElection,
    SecretDecrypt,
    DebugExec,
    ConfigChange,
    CertRotation,
    FaultInjection,
    Autoscale,
    Rollback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub identity: String,       // e.g., "alice@myorg", "ci@github"
    pub source: ActorSource,
    pub source_ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActorSource {
    Cli,
    Api,
    GitOps { commit: String },
    Autoscaler,
    AutoRollback,
    System,
}
```

### Plan and Diff Results

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanResult {
    pub changes: Vec<PlanChange>,
    pub summary: PlanSummary,
    pub validation_errors: Vec<ValidationError>,
    pub scheduling_preview: Vec<SchedulingDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanChange {
    pub resource: String,           // e.g., "app.web", "job.cleanup"
    pub action: PlanAction,
    pub fields: Vec<FieldChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PlanAction {
    Create,
    Update,
    Destroy,
    NoChange,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldChange {
    pub path: String,               // e.g., "image", "replicas", "env.FEATURE_FLAG"
    pub old_value: Option<String>,
    pub new_value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSummary {
    pub to_create: u32,
    pub to_update: u32,
    pub to_destroy: u32,
    pub unchanged: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulingDecision {
    pub instance: String,           // e.g., "web-4"
    pub target_node: String,        // e.g., "node-02"
    pub constraints_satisfied: Vec<ConstraintCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstraintCheck {
    pub constraint: String,         // e.g., "storage=ssd"
    pub satisfied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffResult {
    pub drifted: Vec<DriftEntry>,
    pub in_sync: Vec<String>,
    pub summary: DiffSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftEntry {
    pub resource: String,
    pub fields: Vec<DriftField>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftField {
    pub path: String,
    pub config_value: String,
    pub cluster_value: String,
    pub cause: Option<String>,      // e.g., "autoscaler adjusted", "manual override"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffSummary {
    pub drifted_count: u32,
    pub in_sync_count: u32,
}
```

### Inspect Output

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectOutput {
    pub resource_type: ResourceType,
    pub name: String,
    pub sections: Vec<InspectSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ResourceType {
    App,
    Node,
    Job,
    Volume,
    Image,
    Route,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectSection {
    pub title: String,
    pub fields: Vec<(String, String)>,
    pub subsections: Vec<InspectSection>,
}
```

### Wtf Diagnosis

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WtfReport {
    pub cluster_name: String,
    pub node_count: u32,
    pub critical: Vec<WtfFinding>,
    pub warnings: Vec<WtfFinding>,
    pub ok: Vec<WtfOk>,
    pub summary: WtfSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WtfFinding {
    pub title: String,
    pub details: Vec<String>,
    pub suggestion: String,
    pub correlated_events: Vec<Event>,
    pub affected_resource: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WtfOk {
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WtfSummary {
    pub critical_count: u32,
    pub warning_count: u32,
    pub ok_count: u32,
}
```

### Trace Result

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceResult {
    pub source: String,
    pub destination: String,
    pub destination_port: u16,
    pub steps: Vec<TraceStep>,
    pub overall_result: TraceVerdict,
    pub latency_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceStep {
    pub step_number: u32,
    pub name: String,               // e.g., "DNS resolution (eBPF)"
    pub details: Vec<String>,
    pub verdict: TraceVerdict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TraceVerdict {
    Pass,
    Fail { reason: String },
}
```

### Cluster Summary

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterSummary {
    pub name: String,
    pub node_count: u32,
    pub app_count: u32,
    pub replica_count: u32,
    pub leader: String,
    pub cpu_percent: f64,
    pub memory_percent: f64,
    pub gpu_used: u32,
    pub gpu_total: u32,
    pub version: String,
    pub council_healthy: bool,
    pub council_members: u32,
}
```

---

## 5. Operations

### Full Command Tree

```
relish                              # Launch the TUI (no arguments)
relish tui                          # The same, explicitly
relish --version                    # Print version and exit
relish --help                       # Print help and exit

# Core operations
relish status                       # Per-instance status table (app, state, PID, restarts)
relish apply <path>                 # Apply a TOML config file or directory
relish apply <path> --dry-run       # Print the plan, contact no agent (always exits 0)
relish deploy <path>                # Health-gated rolling deploy from a config file
relish deploy <path> --dry-run      # Print the plan without deploying
relish rollback <app>               # Roll back an app one step, to its previous version
relish rollback <app> --namespace <ns>  # Roll back within a namespace
relish stop <app>                   # Stop all instances of an app
relish inspect <app>                # Per-instance detail (bare app name, not app.X)

# Configuration tooling
relish compile <path>               # Merge + resolve configs into one TOML document
relish lint <path>                  # Validate a config file (warnings to stderr)
relish fmt <path>                   # Format a TOML file in place
relish fmt <path> --check           # Verify formatting without writing
relish diff <path-a> [<path-b>]     # Structural diff between two configs (b defaults to empty)

# Live debugging
relish logs <app>                   # Stream/print logs for an app
relish logs <app> --tail <n>        # Start with the last N lines
relish logs <app> --follow          # Follow new lines (-f)
relish logs <app> --since <time>    # 30s, 5m, 2h, 1d, or epoch seconds
relish logs <app> --grep <substr>   # Substring filter
relish logs <app> --json-field <k=v> # Structured JSON field match
relish logs <app> --namespace <ns>  # App namespace (default "default")
relish logs-export --dest <dir>     # Export Parquet log files
relish logs-search <dir> <sql>      # SQL over an exported Parquet archive
relish trace <src> --to <dst>       # DNS / service-map / firewall / TCP evidence probe
relish inspect <app>                # Per-instance detail for an app
relish resolve <name>               # Resolve a service to its VIP and backends
relish routes                       # Show the ingress routing table
relish top                          # Workload table: state, PID, restarts (no live CPU/memory)
relish wtf                          # Correlated cluster health diagnosis
relish wtf --app <app>              # Scope to one app
relish wtf --watch                  # Re-run every 30s until Ctrl-C

# Forensics
relish history <app>                # Deploy history for an app
relish history <app> --namespace <ns>  # Scope to a namespace

# Interactive debugging
relish exec <app> <cmd...>          # Run a command inside a running instance
relish exec <app> --namespace <ns> <cmd...>  # Scope to a namespace

# Cluster lifecycle
relish init [dir] --cluster-name <name> --node-id <id> # Generate config + PKI
relish init [dir] --development-plaintext              # Local-only plaintext transports
relish nodes                        # Gossip membership and node state
relish council                      # Raft voters and the current leader
relish council recover --data-dir <dir> [--from <url>] [--master-key <path>] [--force]
relish join --token <token> --node-id <id> <api-addr>  # Enrol a node identity
relish join --token <t> --node-id <id> <addr> --ca-fingerprint sha256:... [--identity-dir <dir>]

# Secrets
relish secret pubkey [dir]          # Print the cluster age public key
relish secret encrypt --pubkey <key> <value> # Encrypt a value for ENC[AGE:...] fields
relish secret rotate                # Start secret-key rotation
relish secret rotate --finalize     # Finalise rotation (drop the old read-only keypair)

# Tokens
relish token create --name <name>   # Create an API token (default role: read-only)
relish token create --name <name> --role <admin|deployer|read-only>
relish token create --name <name> --ttl-days <days>
relish token create --name <name> --apps <list>       # Scope to specific apps
relish token create --name <name> --namespaces <list> # Scope to namespaces
relish token list                   # List all API tokens
relish token revoke <name>          # Revoke a token by name
relish join-token create --node-id <id> [--ttl 15m]   # One single-use node-enrolment token

# Volume snapshots (Btrfs-backed)
relish snapshot create <app> [--volume <path>] [--name <name>] [-n <ns>]
relish snapshot list <app> [-n <ns>]
relish snapshot restore <app> <name> [-n <ns>]
relish snapshot delete <app> <name> [-n <ns>]

# Image registry (Pickle)
relish images                       # List images in the local registry
relish build <path>                 # Build [build.*] images and push to Pickle
relish sign <image>                 # Sign an image and attach the signature

# Jobs / batch
relish batch <path>                 # Submit [job.*] sections as a batch
relish batch-status <id> [--wait] [--timeout <secs>]

# Upgrades
relish upgrade check                # Check for available updates
relish upgrade start <version>      # Start rolling cluster upgrade
relish upgrade start --binary <path> # Upgrade from local binary (air-gapped)
relish upgrade start <version> --parallel <n> # Parallel worker upgrades
relish upgrade plan <version>       # Preview upgrade order and duration
relish upgrade plan <version> --cluster-size <n> # Estimate for large clusters
relish upgrade status               # Show upgrade progress
relish upgrade rollback             # Roll back to previous version
relish upgrade rollback <version>   # Roll back to specific version
relish upgrade resume               # Resume a paused upgrade

# Fault injection (Smoker)
relish fault delay <app> <duration> --acknowledge       # Reserved; rejected until TC ships
relish fault drop <app> <percent> --acknowledge         # Fail percentage of connections
relish fault partition <app> --from <app> --acknowledge # Block traffic between apps
relish fault dns <app> nxdomain --acknowledge           # DNS resolution failure
relish fault bandwidth <app> <rate> --acknowledge       # Reserved; rejected until TC ships
relish fault cpu <app> <percent> --acknowledge          # Consume CPU allocation
relish fault memory <app> <percent> --acknowledge       # Push memory toward memory.high
relish fault disk-io <app> <rate> --acknowledge         # Throttle disk I/O
relish fault kill <instance> --acknowledge              # Kill specific instance
relish fault kill <app> --count <n> --acknowledge       # Kill N random instances
relish fault pause <app> --acknowledge                  # SIGSTOP all instances
relish fault pause <app> --instance <id> --acknowledge  # SIGSTOP one instance
relish fault resume <app> --acknowledge     # SIGCONT (unfreeze)
relish fault node-drain <node> --acknowledge # Withdraw from scheduling, keep transports
relish fault node-kill <node> --acknowledge  # Quiesce gossip, Raft and reporting
relish fault node-kill <node> --duration <d> --acknowledge # Auto-recover after duration
relish fault node-pressure <node> --cpu 80% --memory 90% --acknowledge
relish fault run <file> --acknowledge       # Run scripted chaos scenario
relish fault run <file> --dry-run           # Preview scenario timing
relish fault run <file> --speed <multiplier> --acknowledge # Run at adjusted speed
relish fault list                           # Show all active faults
relish fault clear                          # Remove all workload faults
relish fault clear <app>                    # Remove faults targeting app
relish fault clear <id> --node <node>       # Reverse a node fault

# Testing
relish test                                 # Run full integration test suite
relish test --filter <groups>               # Run specific test groups
relish test --parallel <n>                  # Set concurrency level
relish test --chaos                         # Run chaos suite (interactive confirmation)
relish test --chaos --yes                   # Non-interactive consent for CI
relish test --profile <profile>             # development/full-runc/full-apple/process-grill
relish test --timeout <duration>            # Set test timeout
relish test --output json                   # Machine-readable results
relish test --namespace <rbtest-name>        # Prefix each case's reserved namespace

# Benchmarks
relish bench                                # Run full performance benchmark
relish bench --quick                        # Abbreviated suite for CI (~2 min)
relish bench --compare <file>               # Compare against baseline
relish bench --quick --compare <file>       # Quick bench with regression check
relish bench --output json                  # Machine-readable results
relish bench --disruptive --yes              # Include real leader failure
relish bench --capacity --yes                # Saturate with leased minimal apps

# Kubernetes migration (requires the `kubernetes` build feature)
relish import -f <file>                     # Convert K8s YAML to Reliaburger TOML (stdout)
relish import -f <file> -f <file>           # Convert several files (repeatable)
relish import -f <file> --strict            # Exit non-zero if any warnings
relish export -f <file>                     # Convert Reliaburger TOML to Kubernetes YAML

# Dev cluster (Lima VMs)
relish dev create [--nodes N] [--cpus N] [--memory <size>] [--runtime runc|process] [name]
relish dev status | start | stop | destroy [name]
relish dev shell <node>
relish dev test [filter] [--recreate]       # Run Linux-gated tests in a VM
relish dev disk                             # Disk usage in the test VM
relish dev clean                            # Clean build artefacts in the test VM
relish dev keygen --out <dir>               # Generate a release signing keypair
relish dev sign-binary --key <key> <binary> # Sign a binary (.sig envelope)

# Manual, source, setup
relish manual                               # Read the built-in manual (TUI)
relish manual --web [--port <p>]            # Serve the manual as one HTML page
relish manual examples [--dir <dir>]        # Write the embedded example configs
relish source [query]                       # Fuzzy-search the embedded source tree
relish setup [--yes] [--dir <dir>] [--binary-dir <dir>]  # Guided install + starter config

# Global flags (available on all commands)
  --endpoint <url>          # Override Bun API URL / RELIABURGER_ENDPOINT
  --output <format>         # Output format: human, json, yaml
  --token <token>           # API token / RELIABURGER_TOKEN
  --ca-cert <path>          # Cluster root CA / RELIABURGER_CA_CERT
```

### Planned commands (not yet implemented)

The following appear in earlier drafts of this document but are not in the
shipped CLI. **Status: planned — not yet implemented.**

- `relish scale <app> <n>` — no imperative scale; set `replicas` in config and `apply`.
- `relish plan <path>` — use `relish apply <path> --dry-run`.
- `relish events` — event streaming exists only as a TUI/dashboard view, not a CLI command.
- `relish route <hostname>` — only `relish routes` (no argument) exists.
- `relish firewall <app>` / `relish firewall test` — no firewall-inspection command; use `relish trace`.
- `relish identity <app>` — no workload-identity command.
- `relish ca status | rotate | revoke` — no CA-management command.
- `relish pickle gc` — no registry garbage-collection command.
- `relish token rotate <name>` — token has only `create`, `list`, `revoke`.
- `relish volume snapshot | snapshots | restore` — the real surface is `relish snapshot create | list | restore | delete`.
- `relish completions <shell>` — no shell-completion generator (no `clap_complete`).
- `relish login` — no login/keychain flow; authenticate with `--token` / `RELIABURGER_TOKEN`.
- `relish exec --debug | --privileged | --node` — `exec` runs a command in the app container only.
- `relish rollback --to <version>` — rollback goes one step; no version target.
- `relish deploy --continue` — no continue flag.
- `relish import -f -` / `--from-cluster` / `--kubeconfig` / `--output-dir`, and `relish export --format` — the importer reads named files only; export has no `--format`.
- `relish logs --instance <id>` — no per-instance flag.
- `relish resolve --all` — no all-services flag.
- `relish inspect app.<name>` / `node.<name>` — inspect takes a bare app name, not dotted notation.

The delay and bandwidth subcommands deliberately remain parseable so the CLI
contract does not need another migration when the TC data path lands. Today the
server rejects both: the loaded cgroup connect hook can refuse a connection but
cannot sleep or pace packets. Service partition is different. Bun resolves the
named source app to its live cgroup ids, writes exact source/VIP/port keys into
the connect map, and refuses the request when eBPF is unavailable. The numeric
cgroup id in the wire type is server-owned and clients must leave it as zero.
`memory oom` is also refused because a kill cannot be reversed; use a Kill
fault when the experiment needs to exercise restart after abrupt termination.

### Detailed Command Behaviour

#### Core Commands

**`relish status`**

Prints a per-instance status table for the local node's workloads: instance id,
app, namespace, state, PID, and restart count. It contacts the local Bun agent
and exits 0 on success, 1 if the agent is unreachable. There is no separate
"degraded" exit code, and it prints no cluster-wide CPU/memory/GPU summary.

```
$ relish status
INSTANCE             APP             NAMESPACE    STATE      PID        RESTARTS
web-1                web             default      Running    48213      0
worker-1             worker          default      Running    48250      0
```

**`relish apply <path>`**

Reads a TOML config file or directory, resolves it (the same merge logic as
`compile`), and sends it to the local Bun agent. There is no interactive
confirmation prompt and no `--yes` flag. Exits 0 on success, 1 on failure.

With `--dry-run`, it prints the apply plan and contacts no agent — always
exiting 0, even when no agent is running. The plan uses `ApplyPlan`'s display
(see `relish deploy` below for the format).

**`relish deploy <path>`**

Reads a TOML config file, validates it, and triggers a health-gated rolling
deploy through the local Bun agent. It takes a **config path**, not an
`<app> <image>` pair. With `--dry-run` it prints the plan and deploys nothing.
When the agent is unreachable it prints the plan to stdout, warns on stderr,
and exits 1.

The plan is `ApplyPlan`'s display (unchanged resources are hidden):

```
Relish apply plan:

  + app.web
      image     myapp:v1
      replicas  1
      port      8080
      health    /healthz
Plan: 1 to create, 0 to update, 0 to destroy.
```

**`relish scale <app> <n>`** — **Status: planned — not yet implemented.**

There is no imperative scale command. Set `replicas` in the app's config and
re-run `relish apply`.

**`relish rollback <app>`**

Reverts an app to its previous successful deploy through the agent. It rolls
back exactly one step; there is no `--to <version>` target. `--namespace`
scopes the app lookup.

#### Configuration Commands

**`relish compile <path>`**

Resolves a TOML configuration directory into its final, fully-merged form. Applies `_defaults.toml`, merges multi-file configurations, expands all inherited values, and outputs a single merged TOML document representing exactly what would be sent to the cluster. A one-line summary (files merged, app and job counts) is printed to stderr.

This is a purely local operation; it doesn't contact the cluster. It parses and merges files using the same resolution logic that the cluster API uses when receiving a configuration.

**`relish lint <path>`**

Validates configuration files for common errors:

- Duplicate app names across files
- Invalid TOML syntax
- Unknown fields (with "did you mean" suggestions)
- Secret references missing `ENC[...]` wrappers
- Port conflicts
- Missing required fields
- Resource values that parse incorrectly (e.g., CPU range where upper bound exceeds node capacity)
- Glob patterns in `allowed_binaries` without `allow_globs = true`
- `default_egress = "allow"` warnings

Returns exit code 0 when the config parses and validates — any warnings (such as a `run_before` target that doesn't exist) go to stderr — and 1 when parsing or validation fails. There is no separate exit code 2. Suitable for CI pipelines and pre-commit hooks.

**`relish fmt <path>`**

Formats and sorts all TOML files in a directory. Consistent key ordering, consistent whitespace, tables grouped logically. Idempotent -- running it twice produces identical output. Writes files in place. Use `--check` to verify formatting without modifying (exits 1 if any file would change).

#### Change Planning Commands

**`relish plan <path>`** — **Status: planned — not yet implemented.**

There is no `plan` subcommand. Use `relish apply <path> --dry-run` (or
`relish deploy <path> --dry-run`), which prints an `ApplyPlan`: `+` for creates,
`~` for updates, `-` for destroys; unchanged resources are hidden. The
scheduling-preview and capacity-validation described in earlier drafts are not
built — the plan diffs desired config against known state by resource and image
only.

**`relish diff <path-a> [<path-b>]`**

Structural diff between two config files. It does **not** detect cluster drift:
both sides are local TOML. `path-a` is the old config and `path-b` the new one;
omit `path-b` to diff against an empty config (showing everything `path-a`
would add). Purely local — it never contacts the cluster.

#### Debugging Commands

**`relish logs <app>`**

Prints (or streams, with `--follow`) the captured stdout/stderr for an app from
Ketchup on the local node. Flags:

- `--tail <n>`: start with the last N lines.
- `--follow` / `-f`: stream new lines as they appear (off by default).
- `--since <time>`: relative (`30s`, `5m`, `2h`, `1d`) or epoch seconds.
- `--grep <substr>`: substring filter (applied server-side where supported, and client-side).
- `--json-field <key=value>`: keep only JSON log lines where the field equals the value (client-side).
- `--namespace <ns>`: the app's namespace (default `default`).

There is no `--instance` flag and no `--no-follow` (following is opt-in).

**`relish events`** — **Status: planned — not yet implemented.**

Event streaming exists only as a TUI view (the `[e]vents` screen and the web
dashboard), not as a CLI command. There is no `relish events` subcommand or its
`--app` / `--node` / `--type` / `--since` / `--until` / `--severity` filters.
Use `relish history <app>` for an app's audit trail.

**`relish trace <app> --to <app|host>`**

End-to-end connectivity diagnosis. Relish finds a running source instance and
calls `POST /v1/trace` on that node. Bun runs only fixed probe scripts; request
values become positional arguments and never shell syntax. The source image
must provide a POSIX `sh`, `nslookup` and `nc` for every observation to run.
The response contains four steps:

1. **DNS query:** Runs `nslookup` inside the source workload. For an internal service it queries `<app>.<namespace>.internal` and checks that the answer contains the live VIP.
2. **Service and eBPF state:** Reads the userspace service map. On Linux with Onion attached, it also reads the live `backend_map` and requires a healthy kernel backend. Otherwise the userspace result is explicitly `inferred`.
3. **Firewall state:** On Linux with the firewall hooks attached, resolves the source PID to its cgroup and evaluates the live namespace and firewall maps using the same rule as the connect hook. Without those maps the result is `Unknown`, never an invented pass.
4. **TCP probe:** Runs `nc` inside the source workload against the service VIP and selected port and reports observed latency.

Every step labels its evidence `observed`, `inferred` or `unavailable` and its
verdict `Pass`, `Fail` or `Unknown`. `Fail` wins the overall result; incomplete
evidence cannot become green. Exit statuses are 0, 1 and 2 respectively.
Workload probes run on a spawned, bounded task so an eight-second probe timeout
can't stall Bun's command loop. Bun permits at most eight concurrent traces per
node and returns HTTP 429 for the ninth instead of accumulating an unbounded
queue of workload processes. Agent shutdown cancels in-flight probes and
releases their permits immediately.

External destinations require `--port`, an Admin credential, the server-owned
`probe_external_destination` permission and an exact `host:port` entry in
`[testing].external_probe_allowlist`. Unknown and production clusters also
need `allow_protected_mutation = true`. The external TCP result is observed,
but when egress enforcement is active the current kernel maps contain resolved
addresses rather than hostname attribution, so that firewall step remains
`Unknown`.

**`relish inspect <app>`**

Takes a **bare app name** (not dotted `app.X` / `node.X` notation). It lists
each running instance of that app on the local node with: instance id, app,
namespace, state, restart count, and — when present — PID and host port. Node,
job, volume and image inspection are not implemented as `inspect` targets, and
it does not show metrics, ingress, identity or deploy history.

**`relish resolve <name>`**

Queries the service map for one service name. Shows the virtual IP, real
backends (host:port), health status, and which node each instance runs on.
There is no `--all` flag.

```
$ relish resolve redis
redis.internal → 127.128.0.3
  Backends:
    redis-1  10.0.1.5:30891  node-01  healthy
```

**`relish firewall <app>` / `relish firewall test`** — **Status: planned — not yet implemented.**

There is no firewall-inspection command. To observe firewall evidence for a
specific source→destination path, use `relish trace <src> --to <dst>`, whose
firewall step reads the live cgroup namespace and firewall maps on Linux.

**`relish top`**

Prints a one-shot table of the local node's workloads — app, namespace, state,
PID, restarts. Despite the name, it does **not** show live CPU or memory, has no
bar charts, no auto-refresh, and no `--node` / `--sort` / `--gpu` flags. It
contacts the local agent, prints once, and exits.

**`relish wtf`**

Automated diagnosis. Checks the entire cluster and produces a categorised report:

- **CRITICAL:** Crashlooping apps, unresponsive nodes, quorum loss, broken Raft.
- **WARNING:** High disk usage, expiring TLS certificates, CPU throttling, active faults.
- **UNKNOWN:** A required source was unavailable, stale, or cannot expose the
  necessary fact in the current API.
- **OK:** Healthy nodes, healthy quorum, normal eBPF maps, image redundancy met, certificates valid, gossip convergence.

An OK row always rests on an available, timestamped observation. The command
doesn't turn a missing source into success. For example, it diagnoses a
crashloop from restart events inside a 15-minute window rather than a lifetime
restart counter, and reports CPU throttling only from a cgroup throttled-time
delta rather than ordinary CPU usage. JSON and YAML use a versioned report
contract with separate `critical`, `warnings`, `unknown`, and `ok` lists.

The key differentiator: `wtf` doesn't just enumerate problems. It correlates them with recent events, identifies likely root causes, and suggests specific remediation. For example, it links a crashlooping app to a recent deploy and shows the relevant log line, saving the operator from running `logs`, `events`, and `history` separately.

`--app <app>` scopes the check to a single app for deeper, faster diagnosis. `--watch` runs continuously with 30-second refresh -- useful during deploys or incidents.

Exit codes: 0 (all observed checks OK), 1 (criticals found), 2 (warnings or
unknown evidence only).

Collection starts with the configured Bun endpoint. If that node isn't
healthy, the command fails because it has no trustworthy cluster view. It then
uses `/v1/cluster/nodes` and the entry node's API port, transport and
credentials to query every expected node concurrently. Each node request has a
ten-second bound. One failed peer degrades the relevant source and produces an
Unknown row; it doesn't discard facts returned by the other peers.

Desired replicas come from the authenticated `/v1/diagnostics/apps` view, not
from whichever instances happened to answer. The collector compares that
intent with scheduled replicas and the entry node's live resolver state. This
means a service with zero backends stays visible instead of disappearing from
the input. Restart and terminal deploy histories currently come from bounded,
process-local rings, so the collector labels them degraded even when the
current window is empty. Alert status doesn't yet carry application and
namespace labels; an app-scoped alert verdict is therefore Unknown rather than
a cluster result presented as an app result.

Recent logs are not a cluster-wide fishing expedition. The collector fetches
them only for applications with enough timestamped restarts to be crashloop
candidates, caps the number of candidates and lines, and accepts structured
`level=error` or stderr evidence rather than matching the word "error"
anywhere in ordinary output. `--watch` supports human output only; JSON and
YAML are exact single-report contracts.

The authenticated `GET /v1/diagnostics` endpoint supplies local evidence that
doesn't belong in the general metrics summary. It samples cgroup v2
`throttled_usec` twice over a bounded 1–10 second window, attributes configured
data, image, log, metric and volume paths to their containing filesystems, and
returns public X.509 identity, issuer, serial and expiry metadata. Host paths,
certificate bodies and key material never cross the API. A partial inventory
uses `degraded`, carrying both the safe facts and the reason they can't support
an OK verdict. Node leaves currently require a Bun restart to reload, so
certificate rotation remains Unknown even when the leaf itself has plenty of
validity left.

#### Forensics Commands

**`relish history <app>`**

Full audit trail for an app. Every deploy, scale event, config change, restart, health check state transition, alert, and manual action, with timestamps and the actor (user identity, CI pipeline, GitOps commit hash, autoscaler, auto-rollback system).

```
$ relish history payment-service --since 24h

payment-service history (last 24h):

  Feb 12 14:29  alert.critical  oom.kill on node-07 (payment-3)
  Feb 12 14:29  restart         payment-3 restarted (OOM, attempt 4/5)
  Feb 12 13:00  deploy          v2.1.0 → v2.1.1 by ci@github (commit a1b2c3d)
  Feb 11 22:00  autoscale       2 → 3 (cpu > 70% for 5m)
  Feb 11 09:15  deploy          v2.0.9 → v2.1.0 by alice@myorg (relish deploy)
```

#### Interactive Commands

**`relish exec <app> <cmd...>`**

Runs a command inside a running instance of an app via the local Bun agent,
which enters the container's namespaces and executes it. Takes a **bare app
name** and the command as trailing arguments (no `--` separator is required);
`--namespace` scopes the lookup.

The `--debug`, `--privileged` and `--node` variants are **Status: planned — not
yet implemented.** `exec` runs a command in the app's own container only; there
are no debug containers, no privileged firewall bypass, and no host-level
execution.

#### TUI Views

**Dashboard (default)**

The top-level view displayed on launch. Shows:

- Header bar: cluster name, node count, leader identity.
- Apps table: all apps with replicas, status, CPU, memory, GPU. Degraded or crashlooping apps are visually highlighted. Expandable rows show individual failing instances.
- Nodes table (compact): node name, app count, CPU%, MEM%, DISK%, GPU.
- Recent events (last 5-10): timestamp, type, message.
- Active alerts: severity indicator and description.
- Navigation bar: `[a]pps [n]odes [j]obs [e]vents [l]ogs [r]outes [s]earch [?]help [q]uit`.

**Apps view (`a`)**

Full-screen list of all apps with columns: name, replicas (ready/desired), status, CPU, memory, GPU, restarts, uptime. Arrow keys to navigate, Enter to drill into app detail.

**App detail (Enter on an app)**

Detailed view for a single app. Tabbed sections:

- **Overview:** image, replicas, placement, ports, health check config.
- **Instances:** table of all instances with node, port, health, CPU, memory.
- **Logs:** streaming log tail for this app (multiplexed across instances).
- **Metrics:** terminal sparkline charts for CPU, memory, request rate.
- **Deploys:** recent deploy history with version, actor, duration, status.
- **Config:** resolved environment variables, resource limits.

**Nodes view (`n`)**

List of all nodes with columns: name, role (Council/Worker, leader star), apps count, CPU%, MEM%, DISK%, GPU. Enter to drill into node detail showing running apps, disk mounts, eBPF service map size, Pickle cache, gossip peers, council status.

**Jobs view (`j`)**

Running and recent jobs with columns: name, status (running/succeeded/failed), duration, schedule, success rate, queue depth. Enter to see job execution history.

**Events view (`e`)**

Scrollable, filterable event stream. Filter bar at top for app, node, type, severity. Events persist for the full Ketchup retention period. New events appear at the top (or bottom in chronological mode). Press `/` to search within events.

**Logs view (`l`)**

Multiplexed log streaming across all instances of a selected app (or all apps). Instance names are colour-coded. Filter bar for log level, text search, app selection. Toggle follow mode with `f`. This replicates `stern` functionality in the TUI.

**Routes view (`r`)**

Wrapper routing table: external hostnames, TLS certificate status (valid/expiring/expired), backend app, backend count and health. Enter to drill into route detail showing individual backend instances and their health.

**Search (`s`)**

Fuzzy search across apps, nodes, jobs, events, and configuration. Type to filter, arrow keys to navigate results, Enter to jump to the matching resource's detail view.

**Help (`?`)**

Full keyboard shortcut reference. Scrollable.

**TUI Navigation Map**

```
Dashboard ──┬── [a] Apps ──── [Enter] App Detail ──┬── [Tab] Instances
             │                                      ├── [Tab] Logs
             │                                      ├── [Tab] Metrics
             │                                      ├── [Tab] Deploys
             │                                      └── [Tab] Config
             │
             ├── [n] Nodes ── [Enter] Node Detail
             │
             ├── [j] Jobs ─── [Enter] Job Detail
             │
             ├── [e] Events (filterable stream)
             │
             ├── [l] Logs (multiplexed stream)
             │
             ├── [r] Routes ─ [Enter] Route Detail
             │
             ├── [s] Search ─ [Enter] Jump to resource
             │
             └── [?] Help

Navigation:
  [Esc]     Back to previous view
  [q]       Quit (from Dashboard) or back (from sub-view)
  [/]       Search within current view
  [Tab]     Switch tabs (in detail views)
  [Up/Down] Navigate list items
  [Enter]   Drill into selected item
  [PgUp/Dn] Scroll page
  [Home/End] Jump to top/bottom
  [r]       Refresh data immediately
  [:]       Command palette (type CLI commands directly)
```

#### Cluster Lifecycle Commands

**`relish init [dir] --cluster-name <name> --node-id <id>`**

Creates the output directory and writes `reliaburger.toml`, a sample
`app.toml`, the CA hierarchy, sealed root backup, master key, initial security
state and the first node's identity. It prints the one-use join token and root
CA fingerprint. It does **not** start Bun. The operator starts the first node
explicitly with `bun --cluster --config <dir>/reliaburger.toml`; the generated
config requires mTLS unless `--development-plaintext` was explicitly used.

**`relish join --token <token> --node-id <id> <api-address>`**

Contacts the current leader's agent API (normally HTTPS on port 9117), sends a
CSR, validates and consumes the join token, and writes the returned certificate
bundle to `--identity-dir` (default `identity`). `--ca-fingerprint sha256:...`
pins the first exchange. The command doesn't add `[cluster].join`, copy the
cluster master key, start Bun or claim that gossip membership has converged;
those are explicit provisioning and startup steps. Port 9443 is a gossip seed,
not a valid API address for this command.

**`relish join-token create --node-id <id> [--ttl <duration>]`**

Calls `POST /v1/join-token/create` with `ttl_seconds` and `node_id`. `--node-id`
is mandatory: the token is bound to the one node id it may enrol, so it cannot be
replayed to obtain a certificate for another node (a token minted for `node-05`
is refused if used to enrol `node-01`). The command accepts a whole number
followed by `s`, `m` or `h`, defaults to `15m`, and rejects values outside
`1s..=1h` before dispatch. The API requires a user Admin principal (the internal
service principal is not enough), creates the token server-side, commits only
`JoinToken { token_hash, expires_at, consumed: false, attestation_mode, node_id }`
to Raft, then returns the plaintext once:

```json
{
  "token": "rbrg_join_1_...",
  "ttl_seconds": 900,
  "expires_at": 1784310900
}
```

The endpoint is leader-only. A follower or election window returns `503`
without the plaintext, so the operator can retry safely against the current
leader. This is intentionally not a subcommand of `relish token`: those
commands manage long-lived API bearer credentials, not node enrolment.

#### Testing & Benchmarking Commands

**`relish test`**

Runs the selected acceptance profile. Each case owns resources through a
server-side lease and the panic-safe outer runner independently verifies its
release. Namespace isolation alone isn't enough: faults, tokens, images,
mounts and node state aren't namespace-scoped. Unknown and production clusters
are protected by default. The client can't make them writable with an
override flag.

The app-and-namespace-lease foundation is implemented.
`BunClient::create_test_lease` calls
`POST /v1/test/leases`; leased applies send the returned id in
`X-Reliaburger-Test-Lease`; renew and release use
`POST /v1/test/leases/{id}/renew` and `DELETE /v1/test/leases/{id}`. Bun owns
the TTL and cleanup reaper. Standalone ownership is flushed before deploy;
cluster ownership and desired state share one Raft entry. The runner asks for
the case timeout plus its cleanup budget and refuses to start when the server's
maximum is shorter. Pass, failure, panic and timeout release through the same
path. Namespace quota declarations use the same lease. This does not yet make
every resource hermetic: app and namespace ownership is complete, but jobs,
faults, tokens, images, mounts and node state need resource variants.

Container-profile cases use the official BusyBox 1.37.0 OCI index pinned by
digest, not `latest`. The index resolves to `linux/amd64` under runc and
`linux/arm64` under Apple Container. Both runtime gates create, start and
execute the pinned workload; a pull, platform-selection or exec failure fails
the gate. ProcessGrill remains a separate profile and launches `bun testapp`
from each node.

Every case reports exactly one of `Pass`, `Fail`, `Skipped` or `Unknown`.
`Pass` needs directly observed evidence. `Skipped` means a known missing
capability which the selected profile marks optional. A timeout, stale source,
collector error, ambiguous result or uncertain cleanup is `Unknown`, never a
green skip. Full profiles fail on a skipped required case, any `Fail`, any
`Unknown`, or unconfirmed cleanup.

Subsystems tested: scheduling, service discovery, deployments, health checks, secrets & config, firewall, workload identity, ingress, volumes, process workloads, jobs, image registry (Pickle), cluster coordination.

`--parallel <n>` controls concurrency. `--filter <groups>` selects specific
subsystems. `--profile` selects `development`, `full-runc`, `full-apple` or
`process-grill`; full profiles reject a missing required capability, timeout,
unknown evidence or unconfirmed cleanup. `--output json` emits the
schema-versioned report for CI. `--timeout <duration>` sets one inherited
deadline per case.

Before a case runs, Relish fetches Bun's authenticated capability snapshot.
Fresh `available` evidence permits the case, fresh `unavailable` evidence may
produce a typed skip when the selected profile makes it optional, and
`unknown` or expired evidence produces `Unknown` rather than a green skip.
`GET /v1/capabilities/cluster` retains one result per expected node, including
peers which failed authentication, timed out or returned stale evidence.

**`relish test --chaos`**

Runs five serial recovery scenarios: council leader failure, a dead worker
with live replicas, a minority council partition, bounded whole-node pressure,
and a node death during an active rolling deploy. Each scenario uses the
digest-pinned BusyBox workload on runc or Apple Container. ProcessGrill remains
a separate acceptance profile because its host-port model can't restore the
same replica count on two surviving nodes.

This suite refuses before creating a lease unless fresh evidence proves at
least three nodes, the container runtime, node kill and node pressure. Server
policy must grant `provision_isolated_workloads`, `alter_node_state` and
`saturate_capacity`; protected clusters must explicitly enable protected
mutation. An interactive invocation requires the operator to type exactly
`yes`; automation passes `--yes`. Consent grants no role or server operation,
and there is deliberately no client override. Missing destructive
prerequisites fail the invocation rather than becoming green skips.

Chaos cases always run one at a time. The runner refreshes the 15-second
capability evidence after each case enters that serial queue, creates its
server-owned workload lease, and records every injected fault by exact
target-local id, owning node and direct client. Teardown clears those exact
faults newest first, then releases the workload lease. It takes the same path
after failure, timeout or panic; any unconfirmed reversal makes cleanup
`Unknown`. Blanket `fault clear` and `chaos heal` aren't used as ownership
substitutes.

Node drain and kill use an Admin with the server's `alter_node_state` grant
and explicit acknowledgement to withdraw scheduler readiness or
reference-count a shared gossip/Raft/reporting transport gate. Both require a
TTL and reverse automatically. The target node repeats authorisation after
forwarding, and general fault clearing cannot reverse node state. Node fault
IDs remain target-local; a manual clear names the owning node and, after
failure detection removes it from live routing, the operator addresses that
node's management endpoint directly. Real node-scoped resource exhaustion is
available only when a rootful Linux node advertises `NodePressure` and the
server policy grants `saturate_capacity` with non-zero CPU/memory ceilings.
The helper runs in an owned cgroup outside Bun and only one may run per node.
Rootless, Apple and missing-controller cases remain unavailable rather than
green skips.

Ordinary workload faults use a separate gate. The caller needs at least the
Deployer role, `[testing].allowed_operations` must contain
`"inject_workload_faults"`, and every injection command or non-dry-run scenario
needs `--acknowledge`. Admin doesn't override a disabled server operation.
Clearing a workload fault still needs that role and grant, but no destructive
acknowledgement or protected-cluster mutation switch. Bun derives audit
identity from the bearer token, ignores the client body's compatibility value,
and emits stable `fault.injected`/clear actions with credential principal,
target, type and duration.

**`relish bench`**

Deploys leased benchmark workloads, measures the public data plane, confirms
teardown, and produces a schema-versioned report. Measures: scheduler throughput,
service discovery latency, network throughput, deploy speed, state
reconstruction time, image distribution speed and, only when explicitly
requested, cluster capacity.

Each report records topology, hosted-CI evidence and every node's build target,
profile, runtime version, rootless mode, kernel and architecture. Metrics also
record their workload size and method. `--quick` runs an abbreviated suite.
`--compare <file>` flags a direction-aware regression only when it is strictly
greater than 10%. It lists missing metrics rather than inventing a comparison.

Comparison deliberately permits different binary versions and Git SHAs: that
is usually what we are measuring. It refuses unlike topology, target/profile,
runtime, rootless mode, kernel, architecture, quick/capacity mode, units,
direction or workload parameters. Schema 2 rejects unknown fields instead of
silently misreading them. If either report identifies a hosted environment,
the changes remain visible but the verdict is informational; noisy shared
workers must not turn a release red.

Preflight omissions such as a missing optional capability remain typed skips.
Once a suite starts, a timeout, panic, API error or unconfirmed cleanup is a
failure and makes the command exit 1. DNS latency comes from `nslookup` inside
a source workload. Network throughput runs `wget` there against the
destination's fully-qualified `.internal` name, crossing the service VIP
rather than a backend host port.

The reconstruction suite kills the observed council leader, so it runs only
with `--disruptive --yes` and available server-owned node-failure authority.
The capacity suite needs `--capacity --yes`. Those flags record intent; they
don't expand server policy or API roles. Relish marks each saturating apply
with a dedicated acknowledgement header; Bun requires a durable lease, Admin,
`saturate_capacity` and the protected-cluster mutation gate before accepting
it. A plain leased apply cannot accidentally enter the capacity path.

#### Kubernetes Migration

**`relish import -f <path>`**

Converts Kubernetes YAML manifests into Reliaburger TOML configuration. Requires one or more `-f <file>` arguments (repeatable) and is available only in builds with the `kubernetes` feature. It does **not** read directories, stdin (`-f -`), a live cluster (`--from-cluster` / `--kubeconfig`), or write per-app files (`--output-dir`) — those are not implemented. Handles multi-document YAML (separated by `---`). Outputs TOML to stdout.

The importer's core logic is **resource correlation**: grouping related Kubernetes objects into unified Reliaburger resources using the same matching rules Kubernetes itself uses:

1. **Service → Deployment/DaemonSet/StatefulSet**: Match Service `.spec.selector` against workload `.spec.template.metadata.labels`.
2. **Ingress → Service**: Match Ingress backend `.service.name` against Service `.metadata.name`.
3. **HPA → workload**: Match HPA `.spec.scaleTargetRef.name` against workload `.metadata.name`.
4. **ConfigMap/Secret → workload**: Match `envFrom` refs and volume mount refs in the workload's pod spec.
5. **PVC → workload**: Match volume claim names referenced in the workload's pod spec.
6. **ResourceQuota → Namespace**: Match by namespace.

Each correlated group becomes one `[app.*]` block. Uncorrelated resources are converted individually.

**Resource mapping:**

| Kubernetes Resource | Reliaburger Equivalent | Notes |
|---|---|---|
| Deployment | `[app.*]` | `replicas`, `image`, resource limits, deploy strategy |
| DaemonSet | `[app.*]` with `replicas = "*"` | Direct equivalent |
| StatefulSet | `[app.*]` with `volume` | **Warning**: ordering guarantees and stable network IDs lost |
| Service (ClusterIP) | Merged into `[app.*]` `port` | Onion handles discovery automatically |
| Service (NodePort/LoadBalancer) | **Warning** | Suggest Wrapper ingress instead |
| Ingress | `[app.*.ingress]` | `host`, `tls`, path rules preserved |
| HorizontalPodAutoscaler | `[app.*.autoscale]` | `min`, `max`, `metric`, `target` |
| ConfigMap (mounted) | `[[app.*.config_file]]` | Each mount → config_file entry |
| ConfigMap (envFrom) | `[app.*.env]` | Flattened into env block |
| Secret (envFrom) | `[app.*.env]` | Values become `"IMPORT:replace-with-encrypted-value"` |
| Secret (mounted) | `[[app.*.config_file]]` | **Warning**: re-encrypt with `relish secret encrypt` |
| Job | `[job.*]` | `command`, `image` |
| CronJob | `[job.*]` with `schedule` | Cron expression preserved |
| PersistentVolumeClaim | `volume = { path, size }` | **Warning** if StorageClass is not local |
| Namespace | `[namespace.*]` | ResourceQuota fields → quota fields |
| NetworkPolicy | **Warning** | Partial via `allow_from`; complex policies dropped |
| ServiceAccount | **Warning** | Replaced by SPIFFE workload identity |
| RBAC | `[permission.*]` | **Approximate**: K8s verbs mapped to Reliaburger actions |
| PodDisruptionBudget | **Dropped** | `max_unavailable` in deploy config covers this |
| initContainers | `[[app.*.init]]` | Direct mapping |
| Multiple containers/pod | **Warning** | First container imported; sidecars listed in warnings |

**Field-level mapping (Deployment → App):**

| Kubernetes field | Reliaburger field |
|---|---|
| `spec.replicas` | `replicas` |
| `spec.template.spec.containers[0].image` | `image` |
| `spec.template.spec.containers[0].ports[0].containerPort` | `port` |
| `resources.requests.cpu` / `resources.limits.cpu` | `cpu = "request-limit"` |
| `resources.requests.memory` / `resources.limits.memory` | `memory = "request-limit"` |
| `readinessProbe.httpGet.path` | `[app.*.health] path` |
| `env[]` and `envFrom[]` | `[app.*.env]` |
| `nodeSelector` | `[app.*.placement] required` |
| `tolerations` | **Warning** (no equivalent) |
| `strategy.rollingUpdate.maxSurge` | `[app.*.deploy] max_surge` |
| `strategy.rollingUpdate.maxUnavailable` | `[app.*.deploy] max_unavailable` |
| `terminationGracePeriodSeconds` | `[app.*.deploy] drain_timeout` |

**Migration report** (printed to stderr):

```
=== Reliaburger Import Report ===

Converted (14 resources → 4 apps, 2 jobs, 1 namespace):
  ✓ Deployment/web + Service/web + Ingress/web + HPA/web → [app.web]
  ✓ Deployment/api + Service/api → [app.api]
  ✓ DaemonSet/monitoring → [app.monitoring] (replicas = "*")
  ✓ Deployment/redis + Service/redis + PVC/redis-data → [app.redis]
  ✓ CronJob/cleanup → [job.cleanup]
  ✓ Job/db-migrate → [job.db-migrate]
  ✓ Namespace/backend + ResourceQuota/limits → [namespace.backend]

Approximated (review recommended):
  ~ NetworkPolicy/api-ingress → [app.api] allow_from (simplified)
  ~ PVC/redis-data: StorageClass "gp3" → local volume (network storage lost)
  ~ ClusterRole/monitoring → [permission.monitoring] (verb mapping approximate)

Dropped (no Reliaburger equivalent):
  ✗ ServiceAccount/api — replaced by automatic SPIFFE workload identity
  ✗ PodDisruptionBudget/web — drain logic uses max_unavailable from deploy config
  ✗ Pod affinity on Deployment/api — use [app.api.placement] with node labels
  ✗ Sidecar container "envoy" in Deployment/web — not supported in v1
  ✗ Secret/api-tls — TLS handled automatically by Wrapper
```

Exits 0 on success, 1 on error. With `--strict`, exits non-zero if any warnings were generated.

**`relish export -f <path>`**

Converts Reliaburger TOML to Kubernetes YAML (also `kubernetes`-feature only). It takes `-f <file>`; there is no `--format` flag. Produces multi-document YAML (separated by `---`).

**Output mapping:**

| Reliaburger resource | Kubernetes output |
|---|---|
| `[app.*]` | Deployment + Service |
| `[app.*]` with `replicas = "*"` | DaemonSet + Service |
| `[app.*.ingress]` | + Ingress |
| `[app.*.autoscale]` | + HorizontalPodAutoscaler |
| `[[app.*.config_file]]` | + ConfigMap |
| `[app.*.env]` with `ENC[AGE:...]` | + Secret (values base64-encoded, marked as opaque) |
| `[job.*]` | Job |
| `[job.*]` with `schedule` | CronJob |
| `[namespace.*]` | Namespace + ResourceQuota |
| `[permission.*]` | Role + RoleBinding |

Features with no Kubernetes equivalent are listed in the export report: `auto_rollback`, Smoker fault rules, process workloads (`exec`), Franchise configuration, Pickle build jobs, and `run_before` dependency ordering (suggest using Argo Workflows or init containers).

---

## 6. Configuration

### No config file

Relish reads **no config file**. There is no `~/.config/relish/config.toml`, no
`.relish.toml`, no `/etc/relish/config.toml`, and no `$RELISH_CONFIG`.
Everything is set per-invocation by global flags, each with an environment
fallback resolved by `src/relish/client.rs`.

### Global flags and environment variables

| Flag | Env fallback | Meaning |
|------|--------------|---------|
| `--endpoint <url>` | `RELIABURGER_ENDPOINT` | Bun API base URL (default `http[s]://127.0.0.1:9117`; the local API port is **9117**) |
| `--token <token>` | `RELIABURGER_TOKEN` | API bearer token |
| `--ca-cert <path>` | `RELIABURGER_CA_CERT` | Cluster root CA PEM; when set the CLI uses HTTPS |
| `--output <fmt>` | — | `human` (default), `json`, `yaml` |

The endpoint validator requires an explicit host (not `localhost`) and forces
`https` for non-loopback hosts. There is no `RELISH_CLUSTER`,
`RELISH_NAMESPACE`, `RELISH_OUTPUT`, `RELISH_NO_COLOR`, per-command default
namespace from config, or TUI-tuning file — those were never implemented.

### Shell Completions

**Status: planned — not yet implemented.** There is no `relish completions`
command and `clap_complete` is not wired in.

---

## 7. Failure Modes

### API Unreachable

When the agent API is unreachable, Relish prints a clear error:

```
Error: unable to reach the agent at https://10.0.1.5:9117
  Check: is the cluster running? Is this machine on the cluster network?
  Last error: connection refused (10.0.1.5:9117)
```

Exit code 1. Commands that operate purely locally (`compile`, `lint`, `fmt`, `diff`) continue to work without cluster access.

### Partial Results from Fan-Out Queries

Commands like `logs`, `top`, and `wtf` fan out to multiple nodes. When some nodes are unreachable:

- Relish returns partial results with a warning header indicating which nodes failed.
- The output includes a `[partial]` indicator and lists the unreachable nodes.
- Exit code 0 (partial data is still useful), but stderr includes the warning.

```
Warning: 2 of 12 nodes unreachable (node-07, node-11)
         Results below may be incomplete.
```

For `relish wtf`, unreachable nodes are reported as a CRITICAL finding.

### TUI Rendering Issues

- **Terminal too small:** If the terminal is below the minimum usable size (80x24), the TUI displays a message asking the user to resize. It doesn't crash or render garbage.
- **Terminal resize during rendering:** The TUI handles `SIGWINCH` (terminal resize signal) gracefully, re-calculating layouts on the next frame.
- **Lost API connection during TUI session:** The TUI shows a "Disconnected" indicator in the status bar and attempts to reconnect every 5 seconds. Stale data remains visible with a timestamp showing when it was last updated. Manual refresh (`r`) triggers an immediate reconnect attempt.
- **Unicode rendering:** The TUI detects terminal Unicode support and falls back to ASCII indicators (`[OK]`, `[!!]`, `[??]`) when Unicode isn't available.

### WebSocket Disconnection (Streaming)

For streaming commands (`logs --follow`, `exec`), WebSocket disconnections are handled with automatic reconnection:

- `logs`: Reconnect and resume from the last received timestamp. Brief gaps are possible but no data is lost (Ketchup persists everything).
- `exec`: Reconnect isn't possible for interactive sessions. The session terminates with a clear error.

### Authentication Failures

- **Expired token:** Clear error message. Mint a replacement with `relish token revoke <name>` followed by `relish token create --name <name> ...` (there is no `relish token rotate`).
- **Insufficient permissions:** The API returns the required role, and Relish displays it: `Permission denied: requires admin role (you have: deployer)`.
- **Certificate mismatch:** Clear mTLS error with the expected CA fingerprint.

---

## 8. Security Considerations

### Token Storage

API tokens are sensitive credentials. Relish does **not** persist them: there is
no OS keychain integration (no `keyring` crate, no `relish login`) and no CLI
config file. A token reaches the CLI one of two ways:

1. **`--token <token>`** on any command.
2. **`RELIABURGER_TOKEN`** in the environment (the flag overrides it). This is
   the usual path for CI pipelines, where the CI system injects the token.

Tokens are never logged and never sent to any host other than the configured
agent endpoint.

### Exec Permission Model

The shipped `exec` runs a command inside a running instance of an app
(`relish exec <app> <cmd...>`) and requires a `deployer` or `admin` credential.

The escalated variants below are **Status: planned — not yet implemented** — the
design intent is recorded here, but neither the flags nor the debug-container
identity isolation exist in the current CLI:

| Command | Required Role (planned) | Rationale |
|---------|--------------|-----------|
| `relish exec --debug <app>` | `deployer` or `admin` | Debug containers would be isolated (own identity, own firewall rules). |
| `relish exec --debug --privileged <app>` | `admin` only | Privileged debug containers would bypass firewall rules. |
| `relish exec --node <node> -- <cmd>` | `admin` only | Host-level access is the highest privilege level. |

### Audit Logging

Every CLI action that modifies cluster state generates an audit event that includes:

- The operator's identity (from the API token or client certificate).
- The source IP address.
- The exact command and arguments.
- A timestamp.
- The result (success/failure).

These events are visible in `relish history` and in the TUI/web events view. Secret decryption events are logged without exposing secret values.

---

## 9. Performance

### CLI Startup Time

Target: under 50ms from invocation to first API request.

Relish is a statically-linked Rust binary with no runtime dependencies. Startup consists of: argument parsing (clap, <1ms), config file loading (TOML parse, <2ms), API client construction (TLS context, <10ms), and the first HTTP request. The binary is ~15MB and maps into memory quickly on modern systems.

For commands that don't contact the cluster (`compile`, `lint`, `fmt`, `diff`, `--version`), total execution time is typically under 20ms.

### TUI Refresh Rate

- **Render loop:** 10 FPS (100ms per frame). This provides smooth scrolling and responsive keyboard input without excessive CPU usage.
- **Metrics data refresh:** Every 2 seconds (configurable). Fetches CPU, memory, GPU utilisation from Mayo via the cluster API.
- **App/node list refresh:** Every 5 seconds (configurable). Fetches the full app and node list from the Raft state.
- **Event/log streaming:** Real-time via WebSocket. Events appear within 100ms of occurrence.
- **CPU usage (idle TUI):** Under 2% of a single core. The render loop sleeps when no input events or data updates are pending.

### Log Streaming Latency

Log lines from application stdout/stderr to the operator's terminal:

- **Same-node:** <10ms. Ketchup captures the log line, the Bun API streams it to the WebSocket.
- **Cross-node:** <50ms. The log line is captured by Ketchup on the source node, the API fan-out retrieves it and forwards it to the client.
- **Multi-instance multiplexing:** Relish maintains parallel WebSocket connections to each node hosting an instance. Lines are interleaved in timestamp order with a 100ms buffering window to maintain ordering across nodes.

### Plan/Diff Computation

`relish plan` and `relish diff` perform a single API round-trip. The cluster API computes the diff server-side (comparing the submitted configuration against Raft state) and returns the result. Typical response time: 50-200ms depending on the number of resources. The local TOML parsing and merging adds <10ms.

### API Request Overhead

All API requests use HTTP/2 with connection reuse. The mTLS handshake occurs once per session. Subsequent requests on the same connection have ~1ms overhead above the server processing time. For commands that make multiple requests (e.g., `wtf` queries status, events, metrics, and logs), requests are parallelized with `tokio::join!`.

---

## 10. Testing Strategy

### CLI Integration Tests

Each CLI command has integration tests that run against a real cluster (the same test cluster used by `relish test`). Tests are organised by command:

```
tests/
  cli/
    test_status.rs          # relish status output and exit codes
    test_apply.rs           # relish apply with various configs
    test_plan.rs            # relish plan output format, scheduling preview
    test_diff.rs            # relish diff drift detection
    test_compile.rs         # relish compile merge logic
    test_lint.rs            # relish lint error detection
    test_fmt.rs             # relish fmt idempotency
    test_logs.rs            # relish logs streaming, filtering
    test_events.rs          # relish events filtering, time ranges
    test_trace.rs           # relish trace step-by-step output
    test_inspect.rs         # relish inspect resource types
    test_wtf.rs             # relish wtf correlation logic
    test_exec.rs            # relish exec, debug containers
    test_top.rs             # relish top output format
    test_history.rs         # relish history audit trail
    test_firewall.rs        # relish firewall rule display, test
    test_resolve.rs         # relish resolve service map query
    test_route.rs           # relish route ingress display
    test_secret.rs          # relish secret encrypt/rotate
    test_token.rs           # relish token create/list/rotate/revoke
    test_volume.rs          # relish volume snapshot/restore
    test_upgrade.rs         # relish upgrade check/plan/start/status
    test_fault.rs           # relish fault injection commands
    test_test.rs            # relish test (meta-test)
    test_bench.rs           # relish bench output format
    test_global_flags.rs    # --output, --namespace, --cluster, --no-colour
```

**Local-only command tests** (`compile`, `lint`, `fmt`) run without a cluster. They use fixture TOML files in `tests/fixtures/` and verify output against expected snapshots.

**API-dependent command tests** start with a known cluster state (deployed via `relish apply` in a test setup step), run the command, and verify the output format, content, and exit code.

**Output format tests** verify that every command produces valid JSON when `--output json` is used. The JSON is deserialised into the corresponding Rust struct to catch serialisation regressions.

### TUI Snapshot Tests

TUI rendering is tested using terminal snapshot testing (similar to `insta` snapshot testing for Rust). Each test:

1. Constructs a `TuiState` with known data (no API calls).
2. Renders the state to a virtual terminal buffer (ratatui's `TestBackend`).
3. Compares the rendered buffer against a stored snapshot.
4. Fails if the rendering changes unexpectedly.

```rust
#[test]
fn test_dashboard_render() {
    let state = TuiState::with_test_data(TestScenario::HealthyCluster);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|f| render_dashboard(f, &state)).unwrap();
    insta::assert_snapshot!(terminal_to_string(&terminal));
}

#[test]
fn test_dashboard_degraded_app() {
    let state = TuiState::with_test_data(TestScenario::DegradedApp);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|f| render_dashboard(f, &state)).unwrap();
    insta::assert_snapshot!(terminal_to_string(&terminal));
}

#[test]
fn test_apps_view_sorting() {
    let state = TuiState::with_test_data(TestScenario::ManyApps);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|f| render_apps_view(f, &state)).unwrap();
    insta::assert_snapshot!(terminal_to_string(&terminal));
}

#[test]
fn test_small_terminal_warning() {
    let state = TuiState::with_test_data(TestScenario::HealthyCluster);
    let mut terminal = Terminal::new(TestBackend::new(60, 15)).unwrap();
    terminal.draw(|f| render_dashboard(f, &state)).unwrap();
    // Should render "terminal too small" message, not crash
    insta::assert_snapshot!(terminal_to_string(&terminal));
}
```

Snapshot tests cover: dashboard (healthy, degraded, empty cluster), apps view (sorting, selection, empty), nodes view, jobs view, events view (with filters), logs view (multi-instance), routes view, search view, help view, and the "terminal too small" fallback.

### Navigation Tests

TUI navigation is tested by simulating key sequences and verifying the resulting view stack:

```rust
#[test]
fn test_navigation_apps_and_back() {
    let mut state = TuiState::with_test_data(TestScenario::HealthyCluster);
    handle_key(&mut state, KeyCode::Char('a'));
    assert_eq!(state.current_view(), ViewKind::Apps);
    handle_key(&mut state, KeyCode::Esc);
    assert_eq!(state.current_view(), ViewKind::Dashboard);
}

#[test]
fn test_navigation_app_detail_tabs() {
    let mut state = TuiState::with_test_data(TestScenario::HealthyCluster);
    handle_key(&mut state, KeyCode::Char('a'));
    handle_key(&mut state, KeyCode::Enter); // select first app
    assert!(matches!(state.current_view(), ViewKind::AppDetail(_)));
    handle_key(&mut state, KeyCode::Tab);   // next tab
    // verify tab index changed
}
```

---

## 11. Prior Art

### kubectl (Kubernetes CLI)

kubectl is the primary CLI for Kubernetes. It provides imperative and declarative resource management (`apply`, `get`, `describe`, `delete`, `exec`, `logs`). Strengths: comprehensive API coverage, well-documented, extensible via plugins. Weaknesses: no built-in TUI, no `plan` equivalent (limited `diff`), events expire after 1 hour, debugging requires multiple commands and external tools, `describe` output is verbose but poorly correlated.

Reference: [kubectl design](https://kubernetes.io/docs/reference/kubectl/)

**What we borrow:** The `apply` and `exec` interaction model. Operators familiar with `kubectl apply` and `kubectl exec` will find `relish apply` and `relish exec` immediately familiar.

**What we do differently:** Everything else. `relish plan` replaces the blind `apply` workflow. `relish wtf` replaces the manual runbook-based diagnosis. `relish trace` replaces the multi-tool connectivity debugging ritual. Events don't expire after 1 hour. There are no plugins to install.

### k9s (Kubernetes TUI)

k9s is a third-party terminal UI for Kubernetes. It provides a navigable view of cluster resources with keyboard shortcuts, log streaming, and shell access. Strengths: excellent TUI design, real-time updates, keyboard-driven workflow. Weaknesses: separate installation, version skew with kubectl, limited debugging (no trace, no wtf, no plan), relies on the Kubernetes API which lacks some data (no eBPF service maps, no integrated metrics).

Reference: [k9s architecture](https://github.com/derailed/k9s)

**What we borrow:** The navigation model (single-key view switching, drill-down with Enter, back with Esc), the app-centric default view, and the concept of making the TUI the primary operational interface.

**What we do differently:** The TUI is built into the same binary as the CLI and agent. There's no version skew. The TUI has access to data that k9s cannot show: eBPF service maps, Mayo metrics, Ketchup logs, Smoker fault status.

### stern (Multi-Pod Log Streaming)

stern is a third-party tool for multiplexing logs from multiple Kubernetes pods. Strengths: simple, effective multi-pod log streaming with colour-coded instance prefixes. Weaknesses: separate installation, no structured query support, no integration with events or metrics.

Reference: [stern GitHub](https://github.com/stern/stern)

**What we borrow:** The multiplexed log streaming model with per-instance colour coding.

**What we do differently:** `relish logs` is built in, supports structured field queries (`--json-field`), integrates with Ketchup for historical logs with configurable retention, and is available in both CLI and TUI modes.

### Terraform CLI (Plan/Apply)

Terraform pioneered the plan-before-apply workflow for infrastructure. `terraform plan` shows exactly what will change; `terraform apply` executes the plan. Strengths: the plan/apply model is one of the best ideas in infrastructure tooling. Weaknesses: Terraform is a separate tool for infrastructure provisioning, not container orchestration.

Reference: [Terraform CLI docs](https://developer.hashicorp.com/terraform/cli)

**What we borrow:** The entire plan/apply model. `relish plan` is directly inspired by `terraform plan`. The output format (create/update/destroy with `+`/`~`/`-` prefixes) is intentionally similar. `relish plan` also includes scheduling decisions, which Terraform doesn't need but container orchestrators benefit from.

**What we do differently:** `relish plan` also validates scheduling feasibility (sufficient resources, matching labels, node capacity). Kubernetes has nothing equivalent to `terraform plan`.

### Nomad CLI (HashiCorp)

Nomad's CLI provides `nomad plan` (change preview), `nomad alloc status` (allocation inspection), and `nomad alloc exec` (container exec). Strengths: the plan command and the single-binary deployment model. Weaknesses: no built-in TUI, no integrated log streaming, no connectivity debugging.

**What we borrow:** The single-binary philosophy and the plan command.

**What we do differently:** Built-in TUI, integrated log/event streaming, `trace`, `wtf`, debug containers.

### lazydocker

lazydocker is a TUI for Docker that provides container, image, and volume management. Strengths: clean TUI design, useful for single-host Docker. Weaknesses: single-host only, no cluster awareness.

**What we borrow:** The idea that a TUI should be the default interface, not an afterthought.

---

## 12. Libraries & Dependencies

Relish is implemented in Rust. The following crates are used:

| Crate | Version | Purpose |
|-------|---------|---------|
| `clap` (derive) | 4.x | CLI argument parsing, subcommand dispatch, help generation |
| `ratatui` | 0.29.x | Terminal UI rendering framework (widgets, layouts, styles) |
| `crossterm` | 0.28.x | Cross-platform terminal manipulation (raw mode, events, colours, cursor) |
| `nucleo-matcher` | 0.3.x | Fuzzy matching for the TUI search view |
| `reqwest` | 0.12.x | HTTP/2 client with mTLS, connection pooling, streaming |
| `tokio` | 1.x | Async runtime for concurrent API calls, WebSocket streams, and TUI event loop |
| `tokio-tungstenite` | 0.28.x | WebSocket client for streaming logs, events, and exec sessions |
| `serde_yaml` | 0.9.x | YAML serialisation for `--output yaml` mode |
| `serde` | 1.x | Serialisation/deserialisation for config, API responses, output |
| `serde_json` | 1.x | JSON formatting for `--output json` |
| `toml` | 0.8.x | TOML parsing for configuration files and `compile`/`lint`/`fmt` |
| `pulldown-cmark` | 0.13.x | Markdown rendering for `relish manual` |
| `rust-embed` | 8.x | Embedding the manual and bundled source into the binary |
| `anyhow` | 1.x | Error handling with context |
| `thiserror` | 2.x | Typed error definitions for API client errors |
| `rustls` | 0.23.x | TLS implementation (used by reqwest) for mTLS client certs |
| `insta` | 1.x | Snapshot testing for TUI rendering (dev dependency) |

All dependencies are vendored in the release build. The binary is statically linked (musl on Linux, native on macOS) with no runtime shared library dependencies.

---

## 13. Open Questions

### Plugin System for Custom Commands

Should Relish support user-defined commands via a plugin mechanism? Two approaches under consideration:

1. **External binary plugins** (kubectl model): Relish discovers executables named `relish-<name>` on `$PATH` and delegates `relish <name> ...` to them. Simple to implement, but breaks the single-binary philosophy and introduces version/compatibility concerns.

2. **Embedded scripting** (Wasm model): Relish loads `.wasm` plugins from a known directory and executes them in a sandboxed Wasm runtime with access to the API client. Maintains the hermetic binary property but adds complexity and a Wasm runtime dependency.

3. **No plugins:** The built-in command set is comprehensive enough. Custom automation is done via shell scripts that call `relish` with `--output json`. This is the current default position.

Decision deferred until user demand is clearer.

### Shell Completions

Shell completions are not yet implemented (there is no `relish completions` command). If added via `clap_complete`, the open questions would be:

- Should completions include dynamic values (app names, node names) fetched from the cluster API? This adds latency to tab completion but significantly improves usability.
- If yes, should completions be cached locally with a TTL (e.g., 60 seconds) to avoid per-keystroke API calls?
- How should completions behave when the cluster is unreachable (fall back to static completions only)?

Current leaning: implement dynamic completions with a 60-second cache and graceful fallback to static completions when offline.

### Remote TUI (SSH-Based)

Should Relish support running the TUI on a remote machine and forwarding the terminal over SSH? This is already possible (SSH naturally forwards terminal I/O), but there are optimisation questions:

- Should Relish detect SSH sessions and reduce rendering frequency to accommodate latency?
- Should there be a `relish serve-tui` mode that hosts a shared TUI session accessible by multiple operators (useful for incident response where multiple people want to see the same dashboard)?
- Should the TUI support a web-based rendering mode (e.g., via xterm.js) as an alternative to SSH? (Note: this overlaps with Brioche, the web UI.)

Current leaning: SSH works naturally. No special mode needed. Shared dashboards are Brioche's domain.

### Command Palette in TUI

The TUI includes a command palette (`:` key) that allows typing CLI commands directly within the TUI. Open questions:

- Should the command palette support the full CLI command set, or only a subset relevant to the current view?
- Should command palette results replace the current view, or open in a split pane?
- Should the command palette have its own history and autocomplete?

Current leaning: full command set with view replacement and history. Autocomplete deferred.

### Offline Mode

Should `relish compile`, `relish lint`, and `relish fmt` work entirely offline without any cluster context? Currently they do (they only parse local TOML files). But should `relish plan` support an offline mode with a cached cluster state snapshot? This would allow operators to preview changes while disconnected.

Current leaning: not for v1. `plan` requires live cluster state to be meaningful. A stale snapshot could produce misleading results.

### Multi-Cluster Support

Should Relish support managing multiple clusters from a single config file (similar to kubectl contexts)? If so:

- Config syntax: `[clusters.prod]`, `[clusters.staging]` sections?
- Switching: `relish context use prod` or `--cluster prod` flag?
- TUI: cluster selector in the dashboard header?

Current leaning: multiple `[clusters.*]` sections with `--cluster <name>` flag and a `relish context` subcommand. TUI cluster switching via `:context <name>` command palette.
