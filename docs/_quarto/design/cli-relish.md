# Relish: CLI & TUI Design Document

## 1. Overview

Relish is the CLI and interactive terminal UI for Reliaburger. It's a single Rust binary that replaces five separate tools from the Kubernetes ecosystem: `kubectl` (cluster management), `k9s` (interactive TUI), `stern` (multi-instance log streaming), `kubectl-debug` (debug containers), and `terraform plan` (change previewing). Every debugging, diagnostic, configuration, and operational capability is compiled into the binary. There's nothing to install, no plugins to manage, no separate monitoring stack to query, and no shell scripts to maintain.

Relish operates in two modes:

- **CLI mode:** When invoked with a subcommand (`relish status`, `relish plan production/`, `relish wtf`), it executes the command, prints output, and exits. Suitable for scripting, CI pipelines, and quick one-off operations.
- **TUI mode:** When invoked with no arguments (`relish`), it launches a full-screen interactive terminal UI similar to k9s or htop. This is the primary operational interface for day-to-day cluster management. It provides real-time views of apps, nodes, jobs, events, logs, and routes with keyboard-driven navigation.

Both modes use the same underlying API client and output formatting. Anything visible in the TUI can also be retrieved via CLI commands, and anything scriptable via the CLI is navigable in the TUI.

### Design Principles

1. **Zero-install debugging.** Every diagnostic tool is built in. An on-call engineer with the Relish binary and a valid token can diagnose any cluster issue without installing additional software.
2. **Plan before apply.** Borrowing from Terraform, `relish plan` shows exactly what will change before anything is applied. No more "apply and hope."
3. **Correlation over enumeration.** Commands like `relish wtf` don't just list problems -- they correlate events, link crashloops to recent deploys, and suggest specific remediation.
4. **Scriptable by default.** Every command supports `--output json` for machine consumption, returns meaningful exit codes, and accepts filter flags for CI integration.
5. **Single binary, single version.** Relish is compiled into the same binary as Bun (the node agent). There's no version skew between client and server. The binary self-identifies its version and the cluster API rejects incompatible clients with a clear upgrade message.

---

## 2. Dependencies

Relish is a pure client-side binary. It doesn't run any server processes, doesn't maintain local state beyond a configuration file, and doesn't require network access except to reach the cluster API. All data comes from the cluster.

### Cluster API (all remote operations)

Every Relish command that interacts with the cluster communicates through the Reliaburger cluster API, exposed on port 9443 (mTLS) on every node. The API follows the request routing described in the whitepaper:

- **Read-only requests** (status, inspect, logs, events, top, resolve, firewall, history, wtf) are served by any council member from its local Raft state replica. The receiving node forwards to the nearest council member if it isn't one itself.
- **Write requests** (apply, deploy, scale, rollback, secret encrypt, volume snapshot, fault inject) are forwarded to the leader, which commits via Raft before responding.

Relish can target any node in the cluster. It doesn't need to know which node is the leader.

### Bun Agent (exec, debug containers)

The `relish exec` and `relish exec --debug` commands require the Bun agent running on the target node. Bun handles:

- Container namespace entry for `exec` (entering the container's PID, mount, and network namespaces).
- Debug container lifecycle for `exec --debug` (creating a temporary container in the target's network namespace, issuing a short-lived SPIFFE certificate with a distinct debug identity, and cleaning up on exit).
- Host-level command execution for `exec --node` (running commands directly on the host, gated by admin permission).
- Streaming the exec session's stdin/stdout/stderr over the API WebSocket connection.

### Onion (resolve, trace)

The `relish resolve` and `relish trace` commands query the Onion eBPF service discovery layer:

- `resolve` queries the eBPF service map to show virtual IPs, real backends, health status, and node placement for a given service name.
- `trace` performs end-to-end connectivity diagnosis through the eBPF DNS interception, connect() rewrite, nftables firewall evaluation, and TCP probe layers.

Both commands call the Bun agent API on the relevant node, which reads the eBPF maps from kernel space.

### Mayo (metrics queries)

The `relish top`, `relish inspect`, and TUI resource usage displays query the Mayo time-series database for CPU, memory, GPU, disk, and network metrics. Mayo runs locally on each node. Relish fan-out queries across multiple nodes are aggregated by the council member handling the request.

### Ketchup (log and event queries)

The `relish logs`, `relish events`, and `relish history` commands query the Ketchup log store. Ketchup stores structured events and application logs on each node with configurable retention (default 7 days for events, 7 days raw / 30 days compressed for logs). Relish streams results from multiple nodes via the API's streaming endpoint.

### Wrapper (route queries)

The `relish route` command queries the Wrapper ingress proxy for the current routing table, TLS certificate status, and backend health.

### Sesame (identity and CA queries)

The `relish identity`, `relish ca status`, and `relish ca rotate` commands query the Sesame security subsystem for workload identity certificates, CA hierarchy, and certificate revocation list status.

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
|  |  StatusCmd, PlanCmd, ApplyCmd, LogsCmd,     |                  |
|  |  EventsCmd, TraceCmd, InspectCmd, WtfCmd,   |                  |
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
    |  (any node:9443) |
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

- **HTTP/2** over mTLS (client certificate from `~/.config/relish/client.crt` and `~/.config/relish/client.key`).
- **Token authentication** via `Authorisation: Bearer <token>` header as a fallback when no client certificate is configured.
- **Automatic endpoint discovery:** The client is configured with one or more seed node addresses. It queries the seed for the current cluster topology and caches the council member list for read routing.
- **WebSocket upgrade** for streaming endpoints (logs, events, exec).
- **Retry with backoff** for transient failures (connection refused, 503). Maximum 3 retries with 1s/2s/4s backoff.
- **Request timeout:** 30s default, configurable per-command. Streaming connections have no timeout.

### Output Formatting

Every command supports three output modes via the `--output` flag:

| Mode | Flag | Description |
|------|------|-------------|
| Human | `--output human` (default) | Coloured, aligned terminal output with Unicode symbols |
| JSON | `--output json` | Machine-readable JSON, one object per line for streaming commands |
| Table | `--output table` | ASCII table format using `tabled`, suitable for piping to `column` or `awk` |

Human-readable output uses ANSI colours (auto-detected, disabled when piped). Status indicators use Unicode: checkmarks, crosses, warning triangles, and bullets. Progress bars use `indicatif` for long-running operations (apply, upgrade, bench).

---

## 4. Data Structures

All data structures are Rust structs with `serde::Serialize` and `serde::Deserialize` for JSON serialisation. The TUI state structs also derive `Clone` for snapshot-based rendering.

### CLI Configuration

```rust
/// CLI configuration, loaded from ~/.config/relish/config.toml
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliConfig {
    /// Cluster API endpoint (any node address).
    /// Can be overridden by --cluster flag or RELISH_CLUSTER env var.
    pub cluster: String,

    /// Default output format: "human", "json", or "table".
    pub output: OutputFormat,

    /// Path to client TLS certificate for mTLS authentication.
    pub client_cert: Option<PathBuf>,

    /// Path to client TLS key.
    pub client_key: Option<PathBuf>,

    /// API token for token-based authentication.
    /// Stored in OS keychain when available, falls back to file.
    pub token: Option<String>,

    /// Path to CA certificate bundle for verifying the cluster API.
    pub ca_cert: Option<PathBuf>,

    /// Default namespace for commands that accept --namespace.
    pub namespace: Option<String>,

    /// TUI-specific settings.
    pub tui: TuiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    /// Refresh interval for metrics data (default: 2s).
    pub metrics_refresh_secs: u64,

    /// Refresh interval for app/node lists (default: 5s).
    pub list_refresh_secs: u64,

    /// Maximum number of log lines to buffer per app (default: 10000).
    pub log_buffer_size: usize,

    /// Colour theme: "dark" (default) or "light".
    pub theme: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OutputFormat {
    Human,
    Json,
    Table,
}
```

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
relish                              # Launch TUI (no arguments)
relish --version                    # Print version and exit
relish --help                       # Print help and exit

# Core operations
relish status                       # One-line cluster summary
relish apply <path>                 # Apply configuration directory/file
relish apply <path> --dry-run       # Alias for relish plan
relish deploy <app> <image>         # Quick image update for a single app
relish deploy -f <path>             # Deploy from a TOML file
relish scale <app> <n>              # Set replica count
relish rollback <app>               # Roll back to previous version
relish rollback <app> --to <version> # Roll back to a specific version

# Configuration tooling
relish compile <path>               # Resolve config to final merged form
relish compile <path> --format json # Output as JSON
relish lint <path>                  # Validate config files
relish fmt <path>                   # Format and sort TOML files

# Change planning
relish plan <path>                  # Preview changes before apply
relish diff <path>                  # Detect cluster drift from config

# Live debugging
relish logs <app>                   # Stream logs from all instances
relish logs <app> --instance <id>   # Stream logs from specific instance
relish logs <app> --since <time>    # Historical logs
relish logs <app> --grep <pattern>  # Text search filter
relish logs <app> --json-field <k=v> # Structured field query
relish events                       # Stream all events
relish events --app <app>           # Filter by app
relish events --node <node>         # Filter by node
relish events --type <types>        # Filter by event type (comma-separated)
relish events --since <time>        # Historical events
relish events --until <time>        # End time for range queries
relish events --severity <level>    # Filter by severity
relish trace <app> --to <app>       # End-to-end connectivity diagnosis
relish inspect <resource>           # Deep resource inspection (app.X, node.X)
relish resolve <name>               # Query eBPF service map
relish resolve --all                # Show all service map entries
relish route                        # Show ingress routing table
relish route <hostname>             # Detail for a single route
relish firewall <app>               # Show effective firewall rules
relish firewall --raw               # Show raw nftables rules
relish firewall test --from <a> --to <b> # Test if connection is permitted
relish top                          # Live resource usage dashboard
relish top --node <node>            # Scope to single node
relish top --sort <field>           # Sort by field (cpu, memory, gpu, name)
relish top --gpu                    # Show only GPU workloads
relish identity <app>               # Show workload identity cert and OIDC config

# Forensics
relish history <app>                # Full audit trail
relish history <app> --since <time> # Scoped audit trail
relish wtf                          # Automated cluster health diagnosis
relish wtf --app <app>              # Scoped diagnosis for one app
relish wtf --watch                  # Continuous diagnosis (30s refresh)

# Interactive debugging
relish exec <instance> -- <cmd>     # Run command in container
relish exec --debug <instance>      # Attach debug container
relish exec --debug --privileged <instance> # Debug container with firewall bypass
relish exec --node <node> -- <cmd>  # Run command on host (admin only)

# Cluster lifecycle
relish init                         # Initialise a new cluster (first node)
relish init --import-key <path>     # Init with existing encryption key
relish join --token <token> <addr>  # Join an existing cluster

# Secrets
relish secret pubkey                # Print cluster encryption public key
relish secret pubkey --namespace <ns> # Namespace-scoped public key
relish secret encrypt --pubkey <key> <value> # Encrypt a secret value
relish secret encrypt --pubkey <key> --file <path> # Encrypt a file
relish secret rotate                # Generate new keypair, re-encrypt all secrets

# Tokens
relish token create                 # Create new API token
relish token create --role <role>   # Specify role (admin, deployer, read-only)
relish token create --ttl <dur>     # Set TTL (default 90d)
relish token create --apps <list>   # Scope to specific apps
relish token create --namespaces <list> # Scope to specific namespaces
relish token list                   # List all tokens with last-used and expiry
relish token rotate <name>          # Rotate token with grace period
relish token revoke <name>          # Revoke a token immediately

# Volumes
relish volume snapshot <app>        # Create a CoW volume snapshot
relish volume snapshots <app>       # List available snapshots
relish volume restore <app> --snapshot <id> # Restore from snapshot

# Certificate authority
relish ca status                    # Show CA hierarchy, expiry, key distribution
relish ca rotate                    # Rotate intermediate CAs
relish ca revoke --node <node>      # Revoke a node certificate

# Image registry (Pickle)
relish pickle gc                    # Trigger image garbage collection cluster-wide
relish pickle gc --node <node>      # GC on a specific node
relish inspect <image>              # Image signature status

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
relish fault delay <app> <duration>         # Add latency to connections
relish fault drop <app> <percent>           # Fail percentage of connections
relish fault partition <app> --from <app>   # Block traffic between apps
relish fault dns <app> nxdomain             # DNS resolution failure
relish fault bandwidth <app> <rate>         # Throttle bandwidth
relish fault cpu <app> <percent>            # Consume CPU allocation
relish fault memory <app> <percent|oom>     # Push memory usage / trigger OOM
relish fault disk-io <app> <rate>           # Throttle disk I/O
relish fault kill <instance>                # Kill specific instance
relish fault kill <app> --count <n>         # Kill N random instances
relish fault pause <app>                    # SIGSTOP all instances
relish fault pause <app> --instance <id>    # SIGSTOP one instance
relish fault pause <app> --resume           # SIGCONT (unfreeze)
relish fault node-drain <node>              # Simulate graceful node departure
relish fault node-kill <node>               # Simulate abrupt node failure
relish fault node-kill <node> --duration <d> # Auto-recover after duration
relish fault run <file>                     # Run scripted chaos scenario
relish fault run <file> --dry-run           # Preview scenario timing
relish fault run <file> --speed <multiplier> # Run at adjusted speed
relish fault list                           # Show all active faults
relish fault clear                          # Remove ALL faults
relish fault clear <app>                    # Remove faults targeting app

# Testing
relish test                                 # Run full integration test suite
relish test --filter <groups>               # Run specific test groups
relish test --parallel <n>                  # Set concurrency level
relish test --chaos                         # Run chaos test suite
relish test --chaos --filter <groups>       # Run specific chaos tests
relish test --chaos --override              # Run against production clusters
relish test --timeout <duration>            # Set test timeout
relish test --output json                   # Machine-readable results
relish test --namespace <name>              # Use specific namespace

# Benchmarks
relish bench                                # Run full performance benchmark
relish bench --quick                        # Abbreviated suite for CI (~2 min)
relish bench --compare <file>               # Compare against baseline
relish bench --quick --compare <file>       # Quick bench with regression check
relish bench --output json                  # Machine-readable results

# Kubernetes migration
relish import -f <path>                     # Convert K8s YAML to Reliaburger TOML
relish import -f <dir>/                     # Import all YAML files in a directory
relish import -f - --dry-run                # Dry run from stdin (report only)
relish import --from-cluster --kubeconfig <path> --namespace <ns>  # Import from live cluster
relish import -f <path> --output-dir <dir>  # Write per-app TOML files to directory
relish import -f <path> --strict            # Exit non-zero if any warnings
relish export --format kubernetes -f <path> # Export to Kubernetes manifests

# Global flags (available on all commands)
  --cluster <addr>          # Override cluster endpoint
  --output <format>         # Output format: human, json, table
  --namespace <ns>          # Target namespace
  --token <token>           # API token (overrides config)
  --no-colour                # Disable ANSI colours
  --verbose                 # Enable debug logging
  --timeout <duration>      # Request timeout
```

### Detailed Command Behaviour

#### Core Commands

**`relish status`**

Prints a one-line cluster summary: cluster name, node count, app count, leader identity, overall health. Exits 0 if healthy, 1 if degraded, 2 if unreachable.

```
$ relish status
prod: 12 nodes, 17 apps, 89 replicas | leader: node-03 | CPU 43% MEM 58% GPU 4/8 | healthy
```

**`relish apply <path>`**

Reads a TOML configuration directory or file, resolves it (same as `compile`), sends it to the cluster API, and waits for convergence. Prints a plan first (same output as `relish plan`) and prompts for confirmation unless `--yes` is passed. Shows a progress bar during rollout. Exits 0 on success, 1 on failure, and prints the failing step.

When `--dry-run` is passed, behaves identically to `relish plan`.

**`relish deploy <app> <image>`**

Quick image update for a single app. Equivalent to modifying the image field in the app's TOML and running `apply`, but without needing the config file. Triggers a rolling deployment with the app's configured deploy strategy. Prints deploy progress and waits for completion.

**`relish scale <app> <n>`**

Sets the replica count for an app. The scheduler places (or removes) instances immediately. Prints scheduling decisions (which nodes new replicas are placed on, or which instances are stopped).

**`relish rollback <app>`**

Reverts to the previous deploy. The system maintains a per-app deploy log; rollback re-applies the previous image and configuration. By default rolls back one step. `--to <version>` allows rolling back to any version in the deploy history.

#### Configuration Commands

**`relish compile <path>`**

Resolves a TOML configuration directory into its final, fully-merged form. Applies `_defaults.toml`, merges multi-file configurations, expands all inherited values, and outputs a single sorted TOML document representing exactly what would be sent to the cluster. Each line is annotated with its source file in a trailing comment.

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

Returns exit code 0 (clean), 1 (errors), or 2 (warnings only). Suitable for CI pipelines and pre-commit hooks.

**`relish fmt <path>`**

Formats and sorts all TOML files in a directory. Consistent key ordering, consistent whitespace, tables grouped logically. Idempotent -- running it twice produces identical output. Writes files in place. Use `--check` to verify formatting without modifying (exits 1 if any file would change).

#### Change Planning Commands

**`relish plan <path>`**

The most important debugging command. Compares a configuration file or directory against the current cluster state and produces a detailed execution plan. Nothing is applied.

The plan includes:

- Resources to create, update, destroy, or leave unchanged
- Per-field diffs for updates (old value to new value)
- Scheduling decisions for new replicas (which nodes, which constraints are satisfied)
- Resource capacity validation (sufficient CPU, memory, GPU, matching labels)
- Namespace quota validation
- Error messages for impossible scheduling (e.g., required labels no node satisfies)

Output uses `+` for additions, `-` for removals, `~` for updates, `=` for unchanged.

**`relish diff <path>`**

Detects cluster drift. Unlike `plan` (which compares local files against cluster state to preview a deploy), `diff` identifies when the cluster has diverged from its declared configuration: manual changes, failed deploys with partial state, or autoscaler adjustments. Each drifted field includes a cause annotation when the system can determine how the drift occurred.

#### Debugging Commands

**`relish logs <app>`**

Streams logs from all instances of an app, multiplexed with instance-name prefixes (like `stern`). Supports:

- `--instance <id>`: Filter to a specific instance.
- `--since <time>`: Historical logs (relative like `1h` or absolute like `2026-02-12T14:00:00`).
- `--grep <pattern>`: Server-side text search, only matching lines are streamed.
- `--json-field <key=value>`: Structured field query for apps that output JSON logs.
- `--follow` (default when streaming): Continuously stream new lines.
- `--no-follow`: Print historical logs and exit.
- `--tail <n>`: Start with the last N lines (default 100).

Logs are fetched from Ketchup on the relevant nodes. For multi-instance streaming, Relish opens parallel WebSocket connections to each node hosting an instance and multiplexes the output, prefixing each line with the instance name and a colour for visual distinction.

**`relish events`**

Streams the structured event log. Every scheduling decision, health check state change, deploy step, scale event, node join/leave, leader election, alert firing, OOM kill, and restart is available. Events are stored in Ketchup with configurable retention (default 7 days), not the 1-hour expiry of Kubernetes events.

Filter flags:

- `--app <app>`: Events related to an app.
- `--node <node>`: Events on a specific node.
- `--type <types>`: Comma-separated event types (`deploy,health,alert,secret`).
- `--since <time>`, `--until <time>`: Time range.
- `--severity <level>`: `info`, `warning`, or `critical`.

**`relish trace <app> --to <app|host>`**

End-to-end connectivity diagnosis. Traces the full path a connection would take through the eBPF service discovery layer, showing every step:

1. **DNS resolution (eBPF):** Resolves the destination name through the eBPF DNS interception. Shows whether the service map contains the entry and the virtual IP assigned.
2. **Connect interception (eBPF):** Shows the virtual IP to real backend rewrite, including how many healthy backends exist.
3. **Network path:** Shows the source and destination nodes and the nftables firewall verdict (ACCEPT or DROP, with the matching rule).
4. **TCP probe:** Performs a real SYN handshake and reports latency.

If any step fails, the trace shows exactly where and why: service map entry missing, no healthy backends, nftables rule blocking, TCP connection refused or timed out. This replaces the Kubernetes debugging sequence of checking DNS, endpoints, network policies, kube-proxy, and CNI separately.

**`relish inspect <resource>`**

Deep inspection of any resource. The resource identifier uses dot notation: `app.web`, `node.node-03`, `job.cleanup`, `volume.redis-data`.

For apps, inspect shows: image, replica count and health, placement constraints, ports and virtual IPs, health check configuration, CPU/memory usage (current avg and 24h peak), uptime, instance table (name, node, port, health, CPU, memory), ingress routes with TLS status, SPIFFE identity and certificate expiry, environment variables (encrypted values shown as `ENC[AGE:...]`), recent deploy history, and active alerts.

For nodes, inspect shows: resource utilisation, running apps, disk usage per mount, eBPF service map entry count, Pickle image cache size, gossip peer list, and council membership status.

For images, inspect shows: image signature status, signing key, cosign/sigstore verification, and Pickle replication status.

**`relish resolve <name>`**

Queries the eBPF service map directly. Shows virtual IPs, real backends (host:port), health status, and which node each instance runs on. `--all` shows every service in the map.

```
$ relish resolve redis
redis.internal → 127.128.0.3
  Backends:
    redis-1  10.0.1.5:30891  node-01  healthy
```

**`relish firewall <app>`**

Shows the effective firewall rules for an app: which apps can reach it, which apps it can reach, and the egress allowlist. `--raw` dumps the underlying nftables rules.

**`relish firewall test --from <app> --to <app>`**

Tests whether a connection between two apps would be permitted by the firewall rules, without actually making the connection. Returns a pass/fail verdict with the matching rule.

**`relish top`**

Live resource usage dashboard that updates every 2 seconds. Shows cluster-wide totals and per-app breakdown with ASCII bar charts. Supports `--node` for node-scoped view, `--sort` for custom sorting, `--gpu` for GPU-only view. Press `q` to exit.

**`relish wtf`**

Automated diagnosis. Checks the entire cluster and produces a categorised report:

- **CRITICAL:** Crashlooping apps, unresponsive nodes, quorum loss, broken Raft.
- **WARNING:** High disk usage, expiring TLS certificates, CPU throttling, active faults.
- **OK:** Healthy nodes, healthy quorum, normal eBPF maps, image redundancy met, certificates valid, gossip convergence.

The key differentiator: `wtf` doesn't just enumerate problems. It correlates them with recent events, identifies likely root causes, and suggests specific remediation. For example, it links a crashlooping app to a recent deploy and shows the relevant log line, saving the operator from running `logs`, `events`, and `history` separately.

`--app <app>` scopes the check to a single app for deeper, faster diagnosis. `--watch` runs continuously with 30-second refresh -- useful during deploys or incidents.

Exit codes: 0 (all OK), 1 (criticals found), 2 (warnings only).

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

**`relish exec <instance> -- <cmd>`**

Runs a command inside a running container. Opens a WebSocket to the Bun agent on the target node, which enters the container's PID and mount namespaces and executes the command. Stdin/stdout/stderr are streamed bidirectionally. Terminal size is forwarded for interactive shells.

**`relish exec --debug <instance>`**

Spins up a temporary debug container (nicolaka/netshoot or equivalent with curl, dig, tcpdump, strace, netstat) attached to the target container's network namespace. The debug container:

- Can see the same network interfaces, virtual IPs, and eBPF service map entries as the target app.
- Receives its own temporary SPIFFE identity (`spiffe://prod/ns/default/debug/<instance>-by-<operator>`) with a short-lived certificate, rather than inheriting the target app's identity.
- Is automatically removed when the operator exits.
- All activity is logged as `debug-exec` events with operator identity, source IP, duration, and executed commands.

`--privileged` allows the debug container to bypass firewall restrictions (requires admin role, prominently logged).

**`relish exec --node <node> -- <cmd>`**

Host-level command execution. Requires admin permission. Runs the command directly on the host outside any container namespace. Useful for inspecting eBPF maps (`bpftool map dump`), filesystem state (`df -h`), and system diagnostics.

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

**`relish init`**

Initializes a new cluster on the current node. Generates root CA and intermediate CAs (node, workload, ingress), generates OIDC signing key, generates age encryption keypair, starts the Bun agent, and prints the join token and dashboard URL. Prompts to configure an external signing key for upgrades.

**`relish join --token <token> <address>`**

Joins an existing cluster. Authenticates via the join token, receives a node certificate signed by the Node CA, and begins accepting workloads.

#### Testing & Benchmarking Commands

**`relish test`**

Runs the full integration test suite. Each test creates its own namespace, runs validation, and tears down. Tests are independent, idempotent, and safe to run against production clusters (they don't interfere with existing workloads). Test apps are compiled into the Bun binary.

Subsystems tested: scheduling, service discovery, deployments, health checks, secrets & config, firewall, workload identity, ingress, volumes, process workloads, jobs, image registry (Pickle), cluster coordination.

`--parallel <n>` controls concurrency. `--filter <groups>` selects specific subsystems. `--output json` for CI. `--timeout <duration>` for pipeline time limits.

**`relish test --chaos`**

Combines integration tests with Smoker fault injection to verify cluster recovery. Tests: leader failure, node failure, network partition, resource exhaustion, cascading failure. Requires at least 3 nodes. Includes a confirmation prompt and refuses to run against clusters tagged `environment = production` unless `--override` is passed.

**`relish bench`**

Deploys stress-generation workloads (compiled into Bun), saturates the cluster, collects measurements, tears down, and produces a report. Measures: scheduler throughput, service discovery latency, network throughput, deploy speed, state reconstruction time, image distribution speed, cluster capacity.

`--quick` runs an abbreviated suite (~2 min) for CI. `--compare <file>` detects regressions against a baseline JSON report (flags any metric regressing >10%).

#### Kubernetes Migration

**`relish import -f <path>`**

Converts Kubernetes YAML manifests into Reliaburger TOML configuration. Accepts files, directories, stdin (`-f -`), or a live cluster (`--from-cluster`). Handles multi-document YAML (separated by `---`). Outputs TOML to stdout by default, or writes per-app files to a directory with `--output-dir`.

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

Exits 0 on success, 1 on error. With `--strict`, exits 2 if any warnings were generated.

**`relish export --format kubernetes -f <path>`**

Converts Reliaburger TOML to Kubernetes YAML. Produces multi-document YAML (separated by `---`).

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

### Config File Location

Relish looks for configuration in the following order (first found wins):

1. `$RELISH_CONFIG` environment variable (explicit path).
2. `~/.config/relish/config.toml` (XDG-compliant default).
3. `.relish.toml` in the current directory (project-local).
4. `/etc/relish/config.toml` (system-wide).

### Example Config File

```toml
# ~/.config/relish/config.toml

# Cluster endpoint. Any node address works; Relish discovers the topology.
cluster = "10.0.1.5:9443"

# Default output format: "human", "json", or "table"
output = "human"

# Authentication (token-based)
# Stored in OS keychain when available; file fallback shown here.
token = "rbgr_tok_eyJhbGciOi..."

# mTLS client certificate (alternative to token auth)
# client_cert = "~/.config/relish/client.crt"
# client_key = "~/.config/relish/client.key"

# CA certificate for verifying the cluster API
ca_cert = "~/.config/relish/ca.crt"

# Default namespace
namespace = "default"

[tui]
# Metrics refresh interval (seconds)
metrics_refresh_secs = 2

# App/node list refresh interval (seconds)
list_refresh_secs = 5

# Maximum log lines buffered per app in TUI
log_buffer_size = 10000

# Colour theme: "dark" or "light"
theme = "dark"
```

### Environment Variable Overrides

| Variable | Overrides |
|----------|-----------|
| `RELISH_CLUSTER` | `cluster` |
| `RELISH_TOKEN` | `token` |
| `RELISH_NAMESPACE` | `namespace` |
| `RELISH_OUTPUT` | `output` |
| `RELISH_CA_CERT` | `ca_cert` |
| `RELISH_NO_COLOR` | Disables ANSI colours (set to any value) |

### Shell Completions

Relish generates shell completions via `clap_complete`:

```bash
# Bash
relish completions bash > /etc/bash_completion.d/relish

# Zsh
relish completions zsh > ~/.zfunc/_relish

# Fish
relish completions fish > ~/.config/fish/completions/relish.fish
```

---

## 7. Failure Modes

### API Unreachable

When the cluster API is unreachable (all configured endpoints fail after retries), Relish prints a clear error:

```
Error: unable to reach cluster at 10.0.1.5:9443
  Tried 3 endpoints, all failed (connection refused)
  Check: is the cluster running? Is this machine on the cluster network?
  Last error: connection refused (10.0.1.5:9443)
```

Exit code 1. Commands that operate purely locally (`compile`, `lint`, `fmt`, `completions`) continue to work without cluster access.

### Partial Results from Fan-Out Queries

Commands like `logs`, `events`, `top`, and `wtf` fan out to multiple nodes. When some nodes are unreachable:

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

For streaming commands (`logs --follow`, `events`, `exec`), WebSocket disconnections are handled with automatic reconnection:

- `logs` and `events`: Reconnect and resume from the last received timestamp. Brief gaps are possible but no data is lost (Ketchup persists everything).
- `exec`: Reconnect isn't possible for interactive sessions. The session terminates with a clear error. The debug container (if any) is cleaned up by the Bun agent after a 60-second timeout.

### Authentication Failures

- **Expired token:** Clear error message with suggestion to rotate: `Token expired. Run: relish token rotate <name>`.
- **Insufficient permissions:** The API returns the required role, and Relish displays it: `Permission denied: 'exec --debug --privileged' requires admin role (you have: deployer)`.
- **Certificate mismatch:** Clear mTLS error with the expected CA fingerprint.

---

## 8. Security Considerations

### Token Storage

API tokens are sensitive credentials. Relish uses a tiered storage strategy:

1. **OS keychain (preferred):** On macOS (Keychain), Linux (libsecret/GNOME Keyring), and Windows (Credential Manager), tokens are stored in the OS keychain via the `keyring` crate. `relish login` stores the token; subsequent commands retrieve it transparently.
2. **Config file (fallback):** When no keychain is available (headless servers, containers), the token is stored in `~/.config/relish/config.toml`. The file is created with `0600` permissions. Relish warns on startup if the file has overly permissive permissions.
3. **Environment variable:** `RELISH_TOKEN` overrides all other sources. Useful for CI pipelines where tokens are injected by the CI system.

Tokens are never logged, never included in `--verbose` output, and never sent to nodes other than the target cluster API.

### Exec Permission Model

The `exec` command hierarchy has escalating permission requirements:

| Command | Required Role | Rationale |
|---------|--------------|-----------|
| `relish exec <instance> -- <cmd>` | `deployer` or `admin` | Running commands in containers is a standard operational task. |
| `relish exec --debug <instance>` | `deployer` or `admin` | Debug containers are isolated (own identity, own firewall rules). |
| `relish exec --debug --privileged <instance>` | `admin` only | Privileged debug containers bypass firewall rules. Requires explicit intent. |
| `relish exec --node <node> -- <cmd>` | `admin` only | Host-level access is the highest privilege level. |

All exec sessions are logged as events with: operator identity, source IP, target instance/node, session duration, and for debug containers, all commands executed within the session.

### Debug Container Identity Isolation

Debug containers receive their own temporary SPIFFE identity rather than inheriting the target app's identity. This is a critical security boundary:

- **Identity:** `spiffe://prod/ns/default/debug/<instance>-by-<operator>` (distinct from the app's `spiffe://prod/ns/default/app/<name>`).
- **Certificate:** Short-lived (15 minutes), issued by Bun from the Workload CA. Automatically rotated if the session exceeds 15 minutes.
- **Firewall:** The eBPF firewall treats the debug container as a separate entity. Firewall rules allowing traffic from `app.web` don't automatically allow traffic from `debug/web-1-by-alice`. This prevents a debug session from unintentionally accessing backends that only the target app is authorised to reach.
- **`--privileged` override:** Admin-only flag that instructs the eBPF firewall to permit all traffic from the debug container. Prominently logged and visible in `relish events --type debug-exec`.

### Audit Logging

Every CLI action that modifies cluster state generates an audit event that includes:

- The operator's identity (from the API token or client certificate).
- The source IP address.
- The exact command and arguments.
- A timestamp.
- The result (success/failure).

These events are queryable via `relish events --type <type>` and visible in `relish history`. Secret decryption events are logged without exposing secret values.

---

## 9. Performance

### CLI Startup Time

Target: under 50ms from invocation to first API request.

Relish is a statically-linked Rust binary with no runtime dependencies. Startup consists of: argument parsing (clap, <1ms), config file loading (TOML parse, <2ms), API client construction (TLS context, <10ms), and the first HTTP request. The binary is ~15MB and maps into memory quickly on modern systems.

For commands that don't contact the cluster (`compile`, `lint`, `fmt`, `completions`, `--version`), total execution time is typically under 20ms.

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
| `clap` (derive) | 4.x | CLI argument parsing, subcommand dispatch, help generation, shell completion generation |
| `ratatui` | 0.28.x | Terminal UI rendering framework (widgets, layouts, styles) |
| `crossterm` | 0.28.x | Cross-platform terminal manipulation (raw mode, events, colours, cursor) |
| `reqwest` | 0.12.x | HTTP/2 client with mTLS, connection pooling, streaming |
| `tokio` | 1.x | Async runtime for concurrent API calls, WebSocket streams, and TUI event loop |
| `tokio-tungstenite` | 0.24.x | WebSocket client for streaming logs, events, and exec sessions |
| `tabled` | 0.17.x | Table formatting for `--output table` mode |
| `indicatif` | 0.17.x | Progress bars and spinners for long-running operations (apply, upgrade, bench) |
| `dialoguer` | 0.11.x | Interactive prompts for confirmations (apply, chaos test, upgrade) |
| `serde` | 1.x | Serialisation/deserialisation for config, API responses, output |
| `serde_json` | 1.x | JSON formatting for `--output json` |
| `toml` | 0.8.x | TOML parsing for configuration files and `compile`/`lint`/`fmt` |
| `chrono` | 0.4.x | Timestamp parsing and formatting for events, history, logs |
| `anyhow` | 1.x | Error handling with context |
| `thiserror` | 2.x | Typed error definitions for API client errors |
| `tracing` | 0.1.x | Structured logging for `--verbose` debug output |
| `clap_complete` | 4.x | Shell completion script generation (bash, zsh, fish) |
| `keyring` | 3.x | OS keychain integration for token storage |
| `rustls` | 0.23.x | TLS implementation (used by reqwest) for mTLS client certs |
| `insta` | 1.x | Snapshot testing for TUI rendering (dev dependency) |
| `unicode-width` | 0.2.x | Correct column width calculation for Unicode characters in terminal output |
| `textwrap` | 0.16.x | Text wrapping for long messages in human-readable output |
| `similar` | 2.x | Diff algorithm for `relish diff` human-readable output coloring |

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

Shell completions are generated by `relish completions <shell>` via `clap_complete`. Open questions:

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
