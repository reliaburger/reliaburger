# Brioche: Built-In Web UI

**Component:** Brioche (Web UI)
**Binary:** Compiled into Bun (the node agent)
**Status:** Design document

---

## 1. Overview

Brioche is Reliaburger's built-in web dashboard, compiled directly into the Bun binary and served from every node in the cluster. It replaces the common Grafana + custom-dashboard pattern by providing purpose-built views for the operations that matter most: cluster health, application status, deployment history, ingress routing, GitOps sync state, and alerting.

Because Brioche is embedded in Bun via Rust's `include_bytes!` / `rust-embed` mechanism, there is nothing to install, no sidecar to deploy, and no separate process to manage. Visiting any node's API port in a browser renders the full dashboard. The UI is a single-page application that makes API calls to the same Bun HTTP server; those calls follow Reliaburger's standard read/write routing (reads go to the nearest council member, writes are forwarded to the leader via Raft).

**Core dashboard pages:**

- **Cluster overview** -- total resource usage, node health map, recent deploys, active alerts
- **App detail** -- CPU/memory charts, request rate, error rate, instance status, streaming logs, deploy history, environment variables, current image
- **Node detail** -- resource utilisation, running apps, disk usage, GPU status
- **Ingress overview** -- request volume, latency distribution, error rates by route, TLS certificate status
- **GitOps status** -- sync state, last applied commit, diff preview, sync history
- **Alert dashboard** -- active alerts, alert history, suppression status
- **Jobs view** -- running/completed/failed jobs, batch queue depth, execution duration

Brioche shares the same API surface as the Relish CLI and TUI. Everything visible in Brioche is also available via `relish` commands and vice versa. The dashboard is strictly a presentation layer -- it has no exclusive data sources or side channels.

---

## 2. Dependencies

Brioche depends on the following Reliaburger subsystems:

| Dependency | Role | Interaction |
|------------|------|-------------|
| **Bun** | HTTP server; hosts Brioche's static assets and API endpoints | Brioche's compiled assets are served by Bun's built-in HTTP listener on the API port (default `9443`) |
| **Mayo** | Metrics queries (CPU, memory, disk, GPU, request rate, error rate, custom metrics) | Brioche issues PromQL-compatible queries against the local Mayo TSDB via internal API; cross-node queries fan out through council aggregators |
| **Ketchup** | Log queries and streaming | Brioche opens a streaming connection (WebSocket or SSE) for live log tailing; historical queries hit Ketchup's structured log index |
| **Cluster API** | State queries (apps, nodes, jobs, deploy history, configuration, alert state) | Standard Bun API endpoints; reads are served by any council member from local Raft state |
| **Wrapper** | Ingress routing info (route table, TLS cert status, per-route metrics) | Brioche queries Wrapper's routing table and certificate metadata via internal API |
| **Lettuce** | GitOps sync status (sync state, last commit, diff preview, sync history) | Brioche queries Lettuce's sync state via the cluster API; the GitOps coordinator (a council member) serves this data |
| **Mustard** | Leader and council discovery | Brioche uses gossip-propagated state to display the current leader, council membership, and node liveness |
| **Sesame** | Authentication and TLS | Brioche uses the same API token mechanism as the Relish CLI; all traffic is protected by the cluster's TLS configuration |

**No external dependencies.** Brioche does not require an internet connection, a CDN, or any external service. All static assets (HTML, CSS, JS, fonts, icons) are compiled into the binary.

---

## 3. Architecture

### 3.1 Asset Embedding

Brioche's frontend assets are compiled into the Bun binary at build time using the `rust-embed` crate. This produces a single binary that contains all HTML, CSS, JavaScript, font files, and icons. There is no filesystem dependency at runtime.

```rust
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "brioche/dist/"]
#[prefix = "ui/"]
struct BriocheAssets;
```

At startup, Bun registers a route handler that serves these embedded assets. The handler sets appropriate `Content-Type` headers, `Cache-Control` directives (assets are content-hashed for cache-busting), and compression (`Content-Encoding: gzip` for text assets pre-compressed at build time).

```rust
// In Bun's HTTP server initialisation (axum router)
let app = Router::new()
    .nest("/api/v1", api_routes())
    .fallback(brioche_handler);

async fn brioche_handler(uri: axum::http::Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    // Serve index.html for SPA client-side routing
    let path = if path.is_empty() || !path.contains('.') {
        "ui/index.html"
    } else {
        &format!("ui/{}", path)
    };

    match BriocheAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime.as_ref()),
                    (header::CACHE_CONTROL, cache_control_for(path)),
                    (header::CONTENT_SECURITY_POLICY, CSP_HEADER),
                ],
                content.data,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
```

### 3.2 SPA Routing

Brioche is a single-page application. All navigation occurs client-side. Any request to a path that does not match a static asset or an `/api/` prefix returns `index.html`, allowing the SPA router to handle the URL. This means bookmarking `https://node-03:9443/apps/web` works -- the server returns `index.html`, and the client-side router renders the app detail view for `web`.

URL structure:

```
/                           → Cluster overview
/apps                       → App list
/apps/:name                 → App detail
/apps/:name/logs            → App streaming logs
/apps/:name/deploys         → App deploy history
/nodes                      → Node list
/nodes/:name                → Node detail
/ingress                    → Ingress overview
/ingress/:host              → Ingress route detail
/gitops                     → GitOps status
/alerts                     → Alert dashboard
/jobs                       → Jobs view
/jobs/:id                   → Job detail
/settings                   → Cluster settings (admin only)
```

### 3.3 Request Routing

Brioche runs on every node, but the data it displays comes from the cluster API. The routing follows Reliaburger's standard pattern (Section 7 of the whitepaper):

```
┌──────────┐         ┌──────────────────┐
│  Browser  │────────►│  Bun (any node)  │
└──────────┘         └────────┬─────────┘
                              │
              ┌───────────────┼────────────────┐
              │               │                │
         Static assets   Read requests    Write requests
         (local embed)   (→ nearest       (→ leader
                          council member)   → Raft commit)
```

- **Static asset requests** (`/`, `/apps`, CSS, JS, fonts): Served directly from the embedded assets on the local node. No network hop.
- **API read requests** (`GET /api/v1/*`): The local Bun forwards to the nearest council member if the local node is not a council member. Council members serve from local Raft state.
- **API write requests** (`POST /api/v1/*`, `PUT`, `DELETE`): Forwarded to the current leader, which commits via Raft before responding.
- **Metrics queries** (`GET /api/v1/metrics/query`): Routed to the leader (or council aggregator), which fans out to relevant nodes and merges results.
- **Log streaming** (`WS /api/v1/logs/:app/stream`): The council member multiplexes log streams from the nodes running the requested app.

### 3.4 Frontend Technology

Brioche uses a deliberately lightweight frontend stack. The goal is a small asset bundle (<500KB gzipped total), fast page loads (<200ms), and zero build-time complexity that would burden Reliaburger contributors.

**Chosen approach: HTMX + server-rendered HTML (Askama templates) + lightweight JS for charts.**

Rationale:

- HTMX (~14KB gzipped) provides dynamic page updates without a heavyweight SPA framework. Partial page swaps, WebSocket integration, and server-sent events are supported natively.
- Askama templates are compiled into Rust at build time (type-checked, zero-allocation rendering). This keeps the rendering logic in Rust and avoids a separate Node.js build pipeline.
- Charts are rendered with uPlot (~10KB gzipped) for time-series visualisation. uPlot is the fastest JS charting library for time-series data (handles millions of points) and has no dependencies.
- Minimal custom JavaScript (<5KB) for interactions not covered by HTMX (keyboard shortcuts, theme switching, chart initialisation).

This means:

- No Node.js required in the build pipeline. `cargo build` produces the complete binary including UI.
- No `node_modules`. No webpack/vite/rollup.
- Templates are type-checked at compile time. A missing variable is a compile error.
- The total JS payload is ~30KB gzipped (HTMX + uPlot + custom glue).

```rust
use askama::Template;

#[derive(Template)]
#[template(path = "pages/cluster_overview.html")]
struct ClusterOverviewPage {
    cluster: ClusterOverview,
    alerts: Vec<AlertSummary>,
    recent_deploys: Vec<DeployEvent>,
    node_count: u32,
    app_count: u32,
}
```

```html
<!-- templates/pages/cluster_overview.html -->
{% extends "layout.html" %}
{% block content %}
<div id="cluster-overview"
     hx-get="/api/v1/ui/cluster-overview"
     hx-trigger="every 5s"
     hx-swap="innerHTML">

  <section class="resource-summary">
    <div class="metric-card">
      <span class="label">Nodes</span>
      <span class="value">{{ node_count }}</span>
    </div>
    <div class="metric-card">
      <span class="label">Apps</span>
      <span class="value">{{ app_count }}</span>
    </div>
    <div class="metric-card">
      <span class="label">CPU</span>
      <span class="value">{{ cluster.total_cpu_percent }}%</span>
    </div>
    <div class="metric-card">
      <span class="label">Memory</span>
      <span class="value">{{ cluster.total_memory_percent }}%</span>
    </div>
  </section>

  <section class="node-health-map">
    {% for node in cluster.nodes %}
    <a href="/nodes/{{ node.name }}"
       class="node-tile {{ node.status }}">
      {{ node.name }}
    </a>
    {% endfor %}
  </section>

  <section class="recent-deploys">
    <h3>Recent Deploys</h3>
    {% for deploy in recent_deploys %}
    <div class="deploy-event">
      <span class="app">{{ deploy.app }}</span>
      <span class="transition">{{ deploy.from_version }} -> {{ deploy.to_version }}</span>
      <span class="status {{ deploy.status }}">{{ deploy.status }}</span>
      <time>{{ deploy.timestamp }}</time>
    </div>
    {% endfor %}
  </section>

  <section class="active-alerts"
           hx-get="/api/v1/ui/alerts-summary"
           hx-trigger="every 3s"
           hx-swap="innerHTML">
    {% for alert in alerts %}
    <div class="alert {{ alert.severity }}">
      <span class="name">{{ alert.name }}</span>
      <span class="target">{{ alert.target }}</span>
      <span class="message">{{ alert.message }}</span>
    </div>
    {% endfor %}
  </section>
</div>
{% endblock %}
```

### 3.5 Real-Time Updates

Brioche uses two mechanisms for real-time data:

1. **HTMX polling** for dashboard data that changes on the order of seconds (resource utilisation, app status, alert state). Default interval: 5 seconds, configurable per-section. HTMX handles partial DOM replacement efficiently.

2. **Server-Sent Events (SSE)** for high-frequency streaming data (log tailing, deploy progress, event stream). SSE is preferred over WebSocket because:
   - Unidirectional (server to client) matches the use case.
   - Automatic reconnection is built into the browser's `EventSource` API.
   - Works through HTTP/2 multiplexing without additional connection overhead.
   - HTMX has native SSE support via `hx-ext="sse"`.

```html
<!-- Streaming logs via SSE -->
<div hx-ext="sse"
     sse-connect="/api/v1/logs/web/stream?follow=true"
     sse-swap="message">
  <!-- Log lines appended here in real-time -->
</div>
```

```rust
// Server-side SSE endpoint
async fn log_stream_sse(
    Path(app_name): Path<String>,
    Query(params): Query<LogStreamParams>,
    auth: AuthContext,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = ketchup::stream_logs(&app_name, params.follow, params.filter)
        .await
        .map(|log_line| {
            Ok(Event::default()
                .data(render_log_line_html(&log_line)))
        });
    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
    )
}
```

---

## 4. Data Structures

### 4.1 Core API Response Types

These structs define the data that Brioche's API endpoints return. They are shared between the backend (Rust) and the Askama templates.

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level dashboard state, used by the cluster overview page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardState {
    pub cluster: ClusterOverview,
    pub recent_deploys: Vec<DeployEvent>,
    pub active_alerts: Vec<AlertSummary>,
    pub recent_events: Vec<ClusterEvent>,
    pub fetched_at: DateTime<Utc>,
    /// Staleness indicator: if data is older than this threshold,
    /// the UI shows a warning banner.
    pub stale_threshold_secs: u64,
}

/// Cluster-wide resource summary and node listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterOverview {
    pub cluster_name: String,
    pub node_count: u32,
    pub app_count: u32,
    pub job_count_running: u32,
    pub leader: NodeIdentity,
    pub council_members: Vec<NodeIdentity>,
    pub total_cpu_cores: f64,
    pub used_cpu_cores: f64,
    pub total_cpu_percent: f32,
    pub total_memory_bytes: u64,
    pub used_memory_bytes: u64,
    pub total_memory_percent: f32,
    pub total_disk_bytes: u64,
    pub used_disk_bytes: u64,
    pub total_gpu_count: u32,
    pub used_gpu_count: u32,
    pub nodes: Vec<NodeSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub name: String,
    pub address: String,
    pub is_leader: bool,
    pub is_council: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSummary {
    pub name: String,
    pub address: String,
    pub status: NodeStatus,
    pub cpu_percent: f32,
    pub memory_percent: f32,
    pub disk_percent: f32,
    pub app_count: u32,
    pub gpu_count: u32,
    pub gpu_used: u32,
    pub uptime_secs: u64,
    pub roles: Vec<NodeRole>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeStatus {
    Healthy,
    Degraded,
    Unreachable,
    Draining,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeRole {
    Leader,
    Council,
    Worker,
    ReadReplica,
    GitOpsCoordinator,
}
```

### 4.2 App Detail Types

```rust
/// Full detail view for a single application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppDetail {
    pub name: String,
    pub namespace: String,
    pub image: String,
    pub status: AppStatus,
    pub replicas_desired: u32,
    pub replicas_running: u32,
    pub replicas_healthy: u32,
    pub cpu_request: ResourceRange,
    pub memory_request: ResourceRange,
    pub gpu_request: Option<u32>,
    pub instances: Vec<AppInstance>,
    pub deploy_history: Vec<DeployEvent>,
    pub environment: HashMap<String, EnvValue>,
    pub ingress: Option<IngressConfig>,
    pub health_endpoint: Option<String>,
    pub metrics_endpoint: Option<String>,
    pub autoscale: Option<AutoscaleConfig>,
    pub current_alerts: Vec<AlertSummary>,
    pub alert_overrides: HashMap<String, AlertOverride>,
    pub created_at: DateTime<Utc>,
    pub last_deployed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AppStatus {
    Healthy,
    Degraded { reason: String },
    CrashLoop { restart_count: u32, window_secs: u64 },
    Deploying { progress_percent: u8 },
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceRange {
    pub min: String,  // e.g., "100m"
    pub max: String,  // e.g., "500m"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInstance {
    pub id: String,           // e.g., "web-3"
    pub node: String,
    pub status: InstanceStatus,
    pub cpu_usage: f32,
    pub memory_usage_bytes: u64,
    pub restart_count: u32,
    pub started_at: DateTime<Utc>,
    pub last_health_check: Option<DateTime<Utc>>,
    pub health_check_status: Option<HealthCheckStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InstanceStatus {
    Running,
    Starting,
    Stopping,
    Crashed { exit_code: i32, reason: String },
    OomKilled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HealthCheckStatus {
    Passing,
    Failing { consecutive_failures: u32 },
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EnvValue {
    Plain(String),
    Encrypted,  // Display "[encrypted]" -- never expose plaintext secrets in the UI
}
```

### 4.3 Deploy History Types

```rust
/// A single deploy event in an app's history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployEvent {
    pub id: String,
    pub app: String,
    pub from_version: String,
    pub to_version: String,
    pub status: DeployStatus,
    pub strategy: DeployStrategy,
    pub initiated_by: DeployInitiator,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub duration_secs: Option<u64>,
    pub rollback_of: Option<String>,  // ID of the deploy this rolled back
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeployStatus {
    InProgress { percent: u8 },
    Completed,
    RolledBack { reason: String },
    Failed { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeployStrategy {
    Rolling,
    BlueGreen,
    Canary { weight_percent: u8 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeployInitiator {
    User { name: String },
    GitOps { commit: String, repo: String },
    Autoscaler,
    Rollback { triggered_by: String },
}
```

### 4.4 Node Detail Types

```rust
/// Full detail view for a single node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeDetail {
    pub name: String,
    pub address: String,
    pub status: NodeStatus,
    pub roles: Vec<NodeRole>,
    pub labels: HashMap<String, String>,
    pub uptime_secs: u64,
    pub bun_version: String,
    pub os_info: String,
    pub kernel_version: String,
    pub cpu: CpuDetail,
    pub memory: MemoryDetail,
    pub disks: Vec<DiskDetail>,
    pub gpus: Vec<GpuDetail>,
    pub network: NetworkDetail,
    pub running_apps: Vec<NodeAppSummary>,
    pub running_jobs: Vec<NodeJobSummary>,
    pub pickle_cache: PickleCacheStatus,
    pub gossip_peers: Vec<GossipPeerStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuDetail {
    pub cores: u32,
    pub usage_percent: f32,
    pub reserved: String,       // e.g., "500m"
    pub allocatable: String,
    pub allocated: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDetail {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub reserved_bytes: u64,
    pub allocatable_bytes: u64,
    pub allocated_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskDetail {
    pub mount_path: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub usage_percent: f32,
    pub purpose: DiskPurpose,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DiskPurpose {
    RaftData,
    Images,
    Logs,
    Metrics,
    Volumes,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuDetail {
    pub index: u32,
    pub model: String,
    pub vram_total_bytes: u64,
    pub vram_used_bytes: u64,
    pub utilization_percent: f32,
    pub temperature_celsius: f32,
    pub assigned_app: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkDetail {
    pub rx_bytes_per_sec: u64,
    pub tx_bytes_per_sec: u64,
    pub connections_active: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeAppSummary {
    pub app_name: String,
    pub instance_id: String,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub status: InstanceStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeJobSummary {
    pub job_name: String,
    pub job_id: String,
    pub status: JobStatus,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickleCacheStatus {
    pub cached_images: u32,
    pub cache_size_bytes: u64,
    pub cache_max_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipPeerStatus {
    pub node_name: String,
    pub last_seen: DateTime<Utc>,
    pub latency_ms: f32,
}
```

### 4.5 Ingress Types

```rust
/// Ingress route as displayed in the ingress overview.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngressRoute {
    pub host: String,
    pub path: Option<String>,
    pub backend_app: String,
    pub tls_mode: TlsMode,
    pub tls_cert_status: TlsCertStatus,
    pub healthy_backends: u32,
    pub total_backends: u32,
    pub request_rate: f64,         // requests/sec (last 1m)
    pub error_rate: f64,           // 5xx responses/sec (last 1m)
    pub p50_latency_ms: f64,
    pub p95_latency_ms: f64,
    pub p99_latency_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TlsMode {
    Acme,
    Cluster,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsCertStatus {
    pub valid: bool,
    pub issuer: String,
    pub expires_at: DateTime<Utc>,
    pub days_until_expiry: i32,
    pub auto_renew: bool,
}
```

### 4.6 GitOps Types

```rust
/// GitOps sync status from Lettuce.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitOpsStatus {
    pub enabled: bool,
    pub repository: String,
    pub branch: String,
    pub path: String,
    pub sync_state: GitOpsSyncState,
    pub last_applied_commit: Option<GitCommit>,
    pub head_commit: Option<GitCommit>,
    pub pending_diff: Option<String>,
    pub poll_interval_secs: u64,
    pub require_signed_commits: bool,
    pub sync_history: Vec<GitOpsSyncEvent>,
    pub coordinator_node: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GitOpsSyncState {
    Synced,
    OutOfSync { drift_description: String },
    SyncInProgress,
    SyncFailed { error: String },
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitCommit {
    pub sha: String,
    pub short_sha: String,
    pub message: String,
    pub author: String,
    pub timestamp: DateTime<Utc>,
    pub signed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitOpsSyncEvent {
    pub id: String,
    pub commit: GitCommit,
    pub status: GitOpsSyncEventStatus,
    pub apps_affected: Vec<String>,
    pub changes_applied: u32,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GitOpsSyncEventStatus {
    Applied,
    Failed { error: String },
    DryRun { diff: String },
    Rejected { reason: String },  // e.g., unsigned commit
}
```

### 4.7 Alert Types

```rust
/// Summary of an active or historical alert.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertSummary {
    pub id: String,
    pub name: String,            // e.g., "cpu.throttled", "oom.kill"
    pub target: AlertTarget,
    pub severity: AlertSeverity,
    pub message: String,
    pub state: AlertState,
    pub fired_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub suppressed: bool,
    pub suppression_reason: Option<String>,
    pub notification_sent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlertTarget {
    App { name: String, instance: Option<String> },
    Node { name: String },
    Cluster,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlertSeverity {
    Critical,
    Warning,
    Info,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlertState {
    Firing,
    Resolved,
    Suppressed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertOverride {
    pub alert_name: String,
    pub app: String,
    pub disabled: bool,
    pub custom_threshold: Option<f64>,
}
```

### 4.8 Job Types

```rust
/// Job view types for batch/cron workloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobOverview {
    pub running: Vec<JobSummary>,
    pub completed_recent: Vec<JobSummary>,  // last 100
    pub failed_recent: Vec<JobSummary>,     // last 100
    pub queue_depth: u64,
    pub throughput_per_minute: f64,
    pub success_rate_percent: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSummary {
    pub id: String,
    pub name: String,
    pub namespace: String,
    pub status: JobStatus,
    pub node: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub duration_secs: Option<u64>,
    pub exit_code: Option<i32>,
    pub retry_count: u32,
    pub max_retries: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JobStatus {
    Queued,
    Running,
    Succeeded,
    Failed { reason: String },
    Retrying { attempt: u32 },
    Cancelled,
}
```

### 4.9 Metrics Query Types

```rust
/// Types for metrics chart data returned to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsQueryResponse {
    pub query: String,
    pub resolution: MetricsResolution,
    pub series: Vec<MetricsSeries>,
    pub fetched_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSeries {
    pub labels: HashMap<String, String>,
    pub timestamps: Vec<i64>,     // Unix millis
    pub values: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetricsResolution {
    Full,          // 10-second intervals (last 24h)
    Downsampled,   // 1-minute aggregates (last 7d)
    Archived,      // 1-hour aggregates (last 90d)
}

/// Chart configuration sent to the frontend for uPlot initialisation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChartConfig {
    pub chart_id: String,
    pub title: String,
    pub y_axis_label: String,
    pub y_axis_unit: ChartUnit,
    pub series_names: Vec<String>,
    pub series_colors: Vec<String>,
    pub time_range: TimeRange,
    pub refresh_interval_secs: u32,
    pub api_endpoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChartUnit {
    Percent,
    Bytes,
    BytesPerSec,
    RequestsPerSec,
    Milliseconds,
    Count,
    Cores,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeRange {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub preset: Option<TimeRangePreset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TimeRangePreset {
    Last15m,
    Last1h,
    Last6h,
    Last24h,
    Last7d,
    Last30d,
    Custom,
}
```

### 4.10 Cluster Event Types

```rust
/// Events displayed in the event stream (shared with Relish TUI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterEvent {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub event_type: EventType,
    pub severity: EventSeverity,
    pub source: String,       // app name, node name, or "cluster"
    pub message: String,
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventType {
    Deploy,
    Scale,
    Alert,
    NodeJoin,
    NodeLeave,
    LeaderElection,
    GitOpsSync,
    CertRotation,
    FaultInjection,
    JobComplete,
    OomKill,
    HealthCheckFail,
    Rollback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventSeverity {
    Info,
    Warning,
    Error,
}
```

---

## 5. Operations

### 5.1 Cluster Overview Page

The default landing page. Provides a single-glance view of cluster health.

**Layout:**

```
┌────────────────────────────────────────────────────────────────────┐
│  Reliaburger · prod                              [leader: node-03] │
├────────────────────────────────────────────────────────────────────┤
│                                                                    │
│  ┌──────┐  ┌──────┐  ┌──────┐  ┌──────┐  ┌──────┐  ┌──────┐    │
│  │Nodes │  │ Apps │  │ CPU  │  │ Mem  │  │ Disk │  │ GPUs │    │
│  │  12  │  │  17  │  │ 45%  │  │ 62%  │  │ 34%  │  │ 4/8  │    │
│  └──────┘  └──────┘  └──────┘  └──────┘  └──────┘  └──────┘    │
│                                                                    │
│  Node Health Map                                                   │
│  ┌──┐ ┌──┐ ┌──┐ ┌──┐ ┌──┐ ┌──┐ ┌──┐ ┌──┐ ┌──┐ ┌──┐ ┌──┐ ┌──┐│
│  │01│ │02│ │03│ │04│ │05│ │06│ │07│ │08│ │09│ │10│ │11│ │12││
│  │██│ │██│ │★█│ │██│ │██│ │▓▓│ │██│ │██│ │██│ │██│ │██│ │██││
│  └──┘ └──┘ └──┘ └──┘ └──┘ └──┘ └──┘ └──┘ └──┘ └──┘ └──┘ └──┘│
│  ██ healthy  ▓▓ degraded  ░░ unreachable  ★ leader                │
│                                                                    │
│  Recent Deploys                        Active Alerts (1)           │
│  ┌──────────────────────────────┐      ┌─────────────────────────┐│
│  │ 14:32 web v1.4.2→v1.4.3  ✓ │      │ ● CRITICAL              ││
│  │ 13:15 api v2.1.0→v2.1.1  ✓ │      │   payment-service:      ││
│  │ 11:40 worker v1.0→v1.1   ✓ │      │   oom.kill (3x/10m)     ││
│  │ 09:22 inference v3.0 ←back │      │                         ││
│  └──────────────────────────────┘      └─────────────────────────┘│
│                                                                    │
│  Event Stream                                                      │
│  14:32:01  deploy   web v1.4.2 → v1.4.3 completed (2m15s)        │
│  14:29:45  alert    payment-service: OOM kill on node-07          │
│  14:28:12  scale    worker 3 → 4 (autoscale, cpu > 70%)          │
│  14:15:00  gitops   sync applied: abc1234 (3 changes)            │
└────────────────────────────────────────────────────────────────────┘
```

**Data sources:**

| Section | API Endpoint | Refresh |
|---------|-------------|---------|
| Resource summary | `GET /api/v1/cluster/summary` | 5s (HTMX poll) |
| Node health map | `GET /api/v1/nodes?fields=name,status` | 5s (HTMX poll) |
| Recent deploys | `GET /api/v1/deploys?limit=10&sort=desc` | 10s (HTMX poll) |
| Active alerts | `GET /api/v1/alerts?state=firing` | 3s (HTMX poll) |
| Event stream | `SSE /api/v1/events/stream` | Real-time |

**Node health map behaviour:** Each node is rendered as a coloured tile. Colour indicates status: green for healthy, yellow for degraded, red for unreachable, blue for draining. A star icon marks the leader. Clicking a tile navigates to the node detail page. At large cluster sizes (>100 nodes), the map switches to a compact grid with tooltip details on hover.

### 5.2 App Detail Page

Accessed via `/apps/:name`. The primary operational view for understanding a single application.

**Sections:**

1. **Header:** App name, namespace, current image tag, overall status badge, replica count (`3/3 healthy`), quick action buttons (Scale, Rollback, Restart).

2. **Resource Charts (uPlot):**
   - CPU usage over time (stacked by instance, with request/limit bands)
   - Memory usage over time (stacked by instance, with limit line)
   - Request rate (requests/sec from Wrapper metrics)
   - Error rate (5xx/sec from Wrapper metrics)
   - Time range selector: 15m, 1h, 6h, 24h, 7d, 30d, custom

   Charts are initialised via a JSON config block rendered into the page, and uPlot fetches data from `/api/v1/metrics/query?expr=...&range=...`. The chart refresh interval is 10 seconds for the active time range.

3. **Instance Table:**

   | Instance | Node | Status | CPU | Memory | Restarts | Uptime |
   |----------|------|--------|-----|--------|----------|--------|
   | web-1 | node-02 | running | 23% | 128Mi | 0 | 4d 12h |
   | web-2 | node-05 | running | 31% | 142Mi | 0 | 4d 12h |
   | web-3 | node-09 | running | 19% | 121Mi | 0 | 2h 15m |

   Each row links to an instance-specific view with per-instance logs and metrics.

4. **Streaming Logs:**
   - SSE-powered log tail, multiplexed across all instances (like `stern`).
   - Colour-coded by instance.
   - Filter by instance, log level, text search.
   - Toggle follow mode (auto-scroll vs. freeze).
   - Log lines are rendered as pre-formatted HTML fragments pushed via SSE.

5. **Deploy History:**
   - Scrollable list of past deployments with version transitions, status, duration, and initiator (user, GitOps commit, autoscaler).
   - Each entry has a "Rollback to this version" button (triggers a write request to the leader).
   - Visual indicators for rollbacks, failures, and in-progress deploys.

6. **Environment:**
   - Table of environment variables. Encrypted values (from `ENC[AGE:...]`) display as `[encrypted]` -- plaintext is never sent to the browser.
   - Indicates source: `_defaults.toml`, app-specific TOML, or runtime override.

7. **Configuration:**
   - Current TOML configuration for the app (read-only).
   - Health check endpoint and last check result.
   - Autoscale configuration (if enabled): min/max replicas, target metric, current scaling decisions.
   - Alert overrides and suppressions.

### 5.3 Node Detail Page

Accessed via `/nodes/:name`.

**Sections:**

1. **Header:** Node name, IP address, status badge, role badges (Leader, Council, Worker), uptime, Bun version.

2. **Resource Charts:**
   - CPU utilisation over time (total, per-core optional)
   - Memory utilisation over time
   - Disk I/O (read/write bytes per second)
   - Network I/O (rx/tx bytes per second)
   - Time range selector

3. **Running Apps Table:**

   | App | Instance | CPU | Memory | Status |
   |-----|----------|-----|--------|--------|
   | web | web-1 | 23% | 128Mi | running |
   | api | api-2 | 45% | 512Mi | running |
   | redis | redis-1 | 8% | 2.1Gi | running |

4. **Disk Usage:**

   | Mount | Purpose | Used | Total | % |
   |-------|---------|------|-------|---|
   | /var/lib/reliaburger/data | Raft | 1.2Gi | 10Gi | 12% |
   | /var/lib/reliaburger/images | Pickle | 23Gi | 50Gi | 46% |
   | /var/lib/reliaburger/logs | Ketchup | 8.4Gi | 20Gi | 42% |
   | /var/lib/reliaburger/metrics | Mayo | 2.1Gi | 5Gi | 42% |

5. **GPU Status** (if GPUs present):

   | GPU | Model | VRAM | Util | Temp | Assigned To |
   |-----|-------|------|------|------|-------------|
   | 0 | A100 80GB | 42/80 GB | 87% | 72C | inference |
   | 1 | A100 80GB | 0/80 GB | 0% | 38C | (idle) |

6. **Gossip Peers:** List of Mustard gossip peers with last-seen timestamp and latency.

7. **Pickle Image Cache:** Number of cached images, cache size, and cache utilisation.

### 5.4 Ingress Overview Page

Accessed via `/ingress`.

**Layout:**

A table of all active ingress routes with real-time traffic metrics, plus a cluster-wide latency distribution chart at the top.

**Cluster-wide charts:**

- Total request volume (req/sec, stacked by route)
- Cluster-wide latency heatmap (y-axis: latency buckets, x-axis: time, colour intensity: request count)
- Error rate (5xx/sec, stacked by route)

**Route table:**

| Host | Path | Backend | TLS | Backends | Req/s | Errors/s | P50 | P95 | P99 | Cert Expires |
|------|------|---------|-----|----------|-------|----------|-----|-----|-----|-------------|
| myapp.com | / | web | ACME | 5/5 | 1.2k | 0.3 | 12ms | 45ms | 120ms | 58d |
| api.myapp.com | /v1 | api | ACME | 3/3 | 890 | 1.1 | 8ms | 22ms | 55ms | 58d |
| dashboard.internal | / | dash | Cluster | 2/2 | 15 | 0 | 5ms | 12ms | 18ms | 340d |

**Visual warnings:**

- Certificate expiring in <14 days: yellow badge
- Certificate expiring in <3 days: red badge
- Error rate >1%: row highlighted in yellow
- Error rate >5%: row highlighted in red
- Unhealthy backends: backend count shown in red

Clicking a route row opens a detail view with per-route time-series charts and the list of backend instances with their individual metrics.

### 5.5 GitOps Status Page

Accessed via `/gitops`.

**Layout:**

1. **Sync Status Banner:**
   - Large status indicator: "Synced" (green), "Out of Sync" (yellow), "Sync Failed" (red), "Sync In Progress" (blue spinner), "Disabled" (gray).
   - Repository URL, branch, path, poll interval.
   - Coordinator node name.

2. **Current State:**
   - Last applied commit: SHA (linked), author, message, timestamp.
   - HEAD commit: SHA, author, message, timestamp.
   - If out of sync: diff preview showing what would change on next sync. The diff is rendered with syntax highlighting (additions in green, removals in red) and grouped by affected app.

3. **Manual Actions (admin only):**
   - "Sync Now" button: triggers an immediate sync poll.
   - "Dry Run" button: shows what the next sync would change without applying.

4. **Sync History:**

   | Time | Commit | Author | Apps Changed | Changes | Status |
   |------|--------|--------|-------------|---------|--------|
   | 14:15 | abc1234 | alice | web, api | 3 | Applied |
   | 12:30 | def5678 | bob | worker | 1 | Applied |
   | 09:00 | 789abcd | ci-bot | inference | 2 | Rejected (unsigned) |

   Each row expands to show the full diff of what was applied/rejected.

### 5.6 Alert Dashboard

Accessed via `/alerts`.

**Layout:**

1. **Active Alerts** (sorted by severity, then by fire time):

   | Severity | Alert | Target | Message | Firing Since | Duration |
   |----------|-------|--------|---------|-------------|----------|
   | CRITICAL | oom.kill | payment-service | OOM kill (3 events in 10m) | 14:29 | 3m |
   | WARNING | cpu.throttled | api | >25% throttled for 5m | 14:20 | 12m |

   Each alert row has actions: "Acknowledge", "Suppress", "View Target".

2. **Alert History** (last 24h, paginated):

   | Time | Alert | Target | Severity | Duration | Resolution |
   |------|-------|--------|----------|----------|------------|
   | 13:45-14:02 | memory.low | web | warning | 17m | Autoscaled |
   | 11:20-11:21 | disk.filling | node-04 | critical | 1m | GC freed space |

3. **Suppression Status:**
   - Table of per-app alert overrides (from TOML configuration).
   - Visual indicator of which default alerts are suppressed for which apps.

4. **Alert Configuration Summary:**
   - Default alert thresholds.
   - Custom alert rules defined in the cluster.
   - Notification destinations (webhook URLs, partially masked).

### 5.7 Jobs View

Accessed via `/jobs`.

**Layout:**

1. **Summary Cards:**
   - Running jobs count
   - Queue depth
   - Throughput (jobs/minute)
   - Success rate (last 1h)

2. **Running Jobs Table:**

   | Job | Namespace | Node | Started | Duration | Retries |
   |-----|-----------|------|---------|----------|---------|
   | etl-daily-20260216 | batch | node-04 | 14:00 | 32m | 0/3 |
   | report-gen-q4 | analytics | node-08 | 14:25 | 7m | 0/1 |

3. **Recent Completed/Failed Jobs** (tabbed view):

   | Job | Status | Node | Duration | Exit | Completed |
   |-----|--------|------|----------|------|-----------|
   | etl-daily-20260215 | succeeded | node-02 | 45m | 0 | yesterday 14:47 |
   | import-users-batch | failed | node-06 | 12m | 1 | 13:30 |

   Failed jobs show the failure reason and offer a "Retry" button (admin only).

4. **Throughput Chart:** Jobs completed per minute over time, with success/failure stacking.

---

## 6. Configuration

### 6.1 Port and Binding

Brioche shares its HTTP listener with the Bun API server. There is no separate port for the web UI.

```toml
# /etc/reliaburger/node.toml

[api]
port = 9443                # Default. Serves both API and Brioche UI.
bind_address = "0.0.0.0"   # Default. Bind to all interfaces.
```

The same port serves:

- `/api/v1/*` -- REST API endpoints (used by Relish CLI, Brioche, CI systems)
- `/*` -- Brioche SPA (static assets and page routes)

There is no configuration to enable or disable Brioche. It is always available on every node because the assets are compiled into the binary. This is intentional: the overhead of embedded static assets is negligible (~500KB in the binary), and having the dashboard available on every node simplifies debugging in production (any node's IP works).

### 6.2 Authentication

Brioche uses the same API token authentication as the Relish CLI (Section 11 of the whitepaper). There is no separate authentication system for the web UI.

**Login flow:**

1. User navigates to any node's Brioche URL.
2. Brioche renders a login page requesting an API token.
3. The user enters their token (generated via `relish token create` or the Brioche UI itself if already authenticated).
4. The token is sent to `POST /api/v1/auth/validate`.
5. On success, the token is stored in an `HttpOnly`, `Secure`, `SameSite=Strict` cookie with the name `rb_session`.
6. Subsequent API requests from the browser include the cookie automatically.

**Token-in-cookie details:**

- The cookie contains the API token encrypted with a per-node session key (derived from the node's identity via HKDF). This prevents cookie theft from being directly useful on a different node, though the cluster's shared token validation means a stolen raw token works anywhere.
- Cookie expiry matches the token's TTL. When the token expires, the cookie is invalidated and the user is redirected to the login page.
- Logout (`POST /api/v1/auth/logout`) clears the cookie.

**OIDC flow (when configured):**

1. User navigates to Brioche.
2. Brioche redirects to the OIDC provider's authorisation endpoint.
3. After authentication, the OIDC provider redirects back to `/auth/callback` with an authorisation code.
4. Bun exchanges the code for an ID token, validates it, and issues a short-lived session cookie.

**Role enforcement:**

- Read-only tokens can view all dashboard pages but cannot trigger actions (deploy, rollback, scale, retry, sync).
- Deployer tokens can trigger deploys, rollbacks, and scaling for apps within their scope.
- Admin tokens have full access including cluster settings, token management, and alert suppression.
- Action buttons are hidden or disabled based on the authenticated token's role.

### 6.3 TLS

Brioche is served over TLS by default. The TLS certificate is the node's API certificate, issued by the cluster's internal CA. For operators accessing Brioche from outside the cluster, they must either:

1. Add the cluster's root CA to their browser's trust store (recommended for teams).
2. Use `tls = "acme"` on an ingress route pointing to Brioche for a publicly-trusted certificate.
3. Accept the browser's certificate warning (acceptable for ad-hoc debugging).

```toml
# Expose Brioche via a publicly-trusted certificate
[app.brioche-proxy.ingress]
host = "dashboard.myorg.com"
tls = "acme"
backend = "internal://brioche"  # Routes to the local Brioche on any node
```

---

## 7. Failure Modes

### 7.1 API Unavailable (Council Unreachable)

**Scenario:** The browser can reach the local Bun instance (for static assets), but the API calls fail because no council member is reachable.

**Behaviour:**

- Brioche displays the last successfully fetched data with a prominent staleness banner: "Data is X minutes old. Cluster API unreachable."
- The staleness banner includes the timestamp of the last successful fetch.
- Charts stop updating but retain their last-rendered state.
- SSE streams disconnect. The UI shows "Log stream disconnected. Reconnecting..." with an automatic retry (exponential backoff: 1s, 2s, 4s, 8s, max 30s).
- Write actions (deploy, scale, rollback) are disabled with a tooltip: "Cluster API unavailable."
- HTMX continues polling at the normal interval; when the API recovers, data refreshes automatically.

**Implementation:**
```rust
/// Response wrapper that includes freshness metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    pub data: T,
    pub fetched_at: DateTime<Utc>,
    pub source: DataSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DataSource {
    Live,
    Cached { original_fetch: DateTime<Utc> },
    Partial { available_nodes: u32, total_nodes: u32 },
}
```

Client-side, the HTMX `afterRequest` event handler inspects the response's `fetched_at` timestamp and shows the staleness banner if the data is older than 30 seconds.

### 7.2 Leader Unreachable (Read-Only Mode)

**Scenario:** Council members are reachable (reads work), but the leader is unreachable (writes fail).

**Behaviour:**

- All read-only pages function normally: cluster overview, app detail (charts, logs, instance status), node detail, ingress overview, alert dashboard.
- Write actions (deploy, scale, rollback, sync-now, retry job) show an error: "Leader unreachable. Write operations are temporarily unavailable."
- The header bar displays a warning: "Read-only mode -- leader election in progress."
- Deploy history and alert history continue to display (they are read from Raft state on council members).
- Once the leader is re-elected, write actions become available automatically.

### 7.3 Metrics Unavailable (Mayo Down)

**Scenario:** The Mayo TSDB on the queried nodes is unavailable or returns errors.

**Behaviour:**

- Charts display a placeholder: "Metrics temporarily unavailable" with a gray dotted border.
- The rest of the page (instance table, deploy history, configuration, logs) continues to function.
- If only some nodes' metrics are unavailable (partial failure), charts render available data with a note: "Partial data -- N of M nodes reporting."

### 7.4 Log Streaming Unavailable (Ketchup Down)

**Scenario:** The Ketchup log stream cannot be established.

**Behaviour:**

- The streaming logs section shows: "Log streaming unavailable. Retrying..."
- Historical log queries (with explicit time ranges) may still work if the Ketchup index is intact.
- The SSE reconnection logic retries with exponential backoff.

### 7.5 Single Node Access During Network Partition

**Scenario:** The operator's browser can only reach one node, which is isolated from the rest of the cluster.

**Behaviour:**

- Brioche serves the SPA (static assets are local).
- If the node is a council member, it serves its last-known Raft state (which may be stale if the partition is prolonged).
- If the node is not a council member, API calls fail and the staleness banner appears.
- The node health map accurately reflects the partition: the local node's gossip state shows other nodes as unreachable.
- This is a legitimate representation of the cluster state from the perspective of the isolated node.

---

## 8. Security Considerations

### 8.1 Authentication

Brioche reuses the cluster's API token system. There are no separate credentials, no default passwords, and no anonymous access.

- **No default admin account.** The first token must be created via `relish token create` on a cluster node. This is intentional: the web UI should not be the bootstrapping path for authentication.
- **Token validation on every request.** The `rb_session` cookie is validated against the Raft-stored token hash on every API request. Expired or revoked tokens are rejected immediately.
- **Rate limiting.** The same per-token rate limit applies (default 100 req/s). Browser polling at 5-second intervals generates approximately 5-10 req/s, well within the limit.

### 8.2 CSRF Protection

All state-mutating API endpoints (POST, PUT, DELETE) require a CSRF token in addition to the session cookie.

**Implementation:**

- On login, the server generates a random CSRF token (256-bit, base64-encoded) and returns it in the response body (not in a cookie).
- The Brioche SPA stores the CSRF token in a JavaScript variable (memory only, not localStorage).
- HTMX is configured to include the CSRF token as a custom header (`X-CSRF-Token`) on all non-GET requests via `hx-headers`.
- The server validates that the `X-CSRF-Token` header matches the token associated with the session.

```html
<body hx-headers='{"X-CSRF-Token": "{{ csrf_token }}"}'>
```

```rust
/// CSRF validation middleware
async fn validate_csrf(
    req: Request<Body>,
    next: Next<Body>,
) -> Result<Response, StatusCode> {
    if req.method() == Method::GET || req.method() == Method::HEAD {
        return Ok(next.run(req).await);
    }

    let session_csrf = extract_session_csrf(&req)?;
    let header_csrf = req
        .headers()
        .get("X-CSRF-Token")
        .and_then(|v| v.to_str().ok());

    match header_csrf {
        Some(token) if constant_time_eq(token, &session_csrf) => {
            Ok(next.run(req).await)
        }
        _ => Err(StatusCode::FORBIDDEN),
    }
}
```

### 8.3 XSS Prevention

- All dynamic content rendered by Askama templates is auto-escaped by default. Askama escapes `<`, `>`, `&`, `"`, and `'` in all template expressions unless explicitly marked with `|safe` (which is used only for trusted, pre-sanitized content like diff syntax highlighting).
- Log lines are sanitized before rendering: all HTML entities are escaped. Logs are rendered inside `<pre>` blocks with no raw HTML interpretation.
- User-supplied values (app names, environment variable keys, commit messages) are always escaped.
- The Content Security Policy (Section 8.4) provides defense in depth against any escaping failures.

### 8.4 Content Security Policy

Brioche serves a strict Content Security Policy header on all responses:

```
Content-Security-Policy:
  default-src 'none';
  script-src 'self';
  style-src 'self' 'unsafe-inline';
  img-src 'self' data:;
  font-src 'self';
  connect-src 'self';
  frame-ancestors 'none';
  base-uri 'self';
  form-action 'self';
```

Breakdown:

- `script-src 'self'`: Only scripts from the embedded assets are executed. No inline scripts, no eval, no external script sources.
- `style-src 'self' 'unsafe-inline'`: Styles from embedded assets, plus inline styles (needed for dynamic chart sizing by uPlot). This is the one concession; `unsafe-inline` for styles has minimal security impact compared to scripts.
- `connect-src 'self'`: XHR/fetch/SSE/WebSocket connections only to the same origin (the local Bun API).
- `frame-ancestors 'none'`: Prevents clickjacking by disallowing embedding in iframes.
- `base-uri 'self'`: Prevents `<base>` tag injection attacks.
- `form-action 'self'`: Forms can only submit to the same origin.

### 8.5 Additional Headers

```rust
fn security_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("X-Content-Type-Options", "nosniff".parse().unwrap());
    headers.insert("X-Frame-Options", "DENY".parse().unwrap());
    headers.insert("Referrer-Policy", "strict-origin-when-cross-origin".parse().unwrap());
    headers.insert("Permissions-Policy",
        "camera=(), microphone=(), geolocation=()".parse().unwrap());
    headers
}
```

### 8.6 Secret Handling

Brioche never displays plaintext secret values. Environment variables that originate from `ENC[AGE:...]` fields are transmitted to the browser as `[encrypted]`. The API endpoint `GET /api/v1/apps/:name/env` returns `EnvValue::Encrypted` for these fields; the plaintext is never included in the API response. This is enforced at the API layer, not just the UI layer, so even a compromised browser or a direct API call cannot retrieve secret values through Brioche.

### 8.7 Management Perimeter

As described in the whitepaper (Section 11.3), the Brioche port (shared with the API port `9443`) is protected by the management perimeter firewall. Only cluster nodes and explicitly authorised administrator CIDR ranges (configured in `node.toml`) can reach it. Traffic from outside these ranges is dropped by nftables before it reaches the Bun HTTP listener.

---

## 9. Performance

### 9.1 Page Load Time

**Target: <200ms** from navigation to fully rendered page (excluding chart data which loads asynchronously).

Breakdown:

- TLS handshake: ~50ms (first load; reused on subsequent navigations)
- HTML response (server-rendered by Askama): <10ms server-side
- Static asset delivery (CSS, JS): <20ms (embedded, gzipped, served from memory with aggressive caching)
- DOM rendering: <50ms (minimal JS, no framework hydration)
- Chart initialisation: async, renders placeholder immediately, data arrives in next 50-100ms

**Asset size budget:**

| Asset | Size (gzipped) | Purpose |
|-------|----------------|---------|
| index.html | <5KB | SPA shell (server-rendered for initial page) |
| styles.css | <15KB | All styles (no CSS framework, hand-written) |
| htmx.min.js | ~14KB | Dynamic page updates |
| uplot.min.js | ~10KB | Time-series charts |
| brioche.js | <5KB | Custom JS (chart init, keyboard shortcuts, theme) |
| fonts | <50KB | System font stack preferred; one fallback font |
| icons | <10KB | SVG icon sprite, inline |
| **Total** | **<110KB** | |

For comparison: a typical Grafana dashboard loads 2-5MB of JavaScript. The Kubernetes Dashboard loads ~1.5MB. Nomad's UI loads ~800KB. Brioche targets <110KB total.

### 9.2 Real-Time Update Intervals

| Data Type | Mechanism | Interval | Rationale |
|-----------|-----------|----------|-----------|
| Cluster overview | HTMX poll | 5s | Balances freshness with API load |
| App instance status | HTMX poll | 5s | Quick crash detection |
| Resource charts | uPlot data fetch | 10s | Matches Mayo's 10s collection interval |
| Active alerts | HTMX poll | 3s | Alerts should appear quickly |
| Log streaming | SSE | Real-time | Operators expect instant log tailing |
| Deploy progress | SSE | Real-time | Deploy status updates are time-sensitive |
| Event stream | SSE | Real-time | Events stream continuously |
| GitOps sync state | HTMX poll | 10s | Sync changes are infrequent |
| Ingress metrics | HTMX poll | 5s | Traffic patterns change continuously |

### 9.3 Server-Side Performance

- **Template rendering:** Askama compiles templates to Rust code at build time. Rendering a cluster overview page with 100 nodes and 50 apps takes <1ms.
- **Metrics query fan-out:** Single-app queries complete in <50ms (fan-out to 3-10 nodes). Cluster-wide queries complete in <500ms (fan-out to 5-7 council aggregators). These numbers are set by Mayo's query architecture, not Brioche.
- **Concurrent dashboard users:** A single Bun instance can serve 100+ concurrent Brioche users without measurable impact. The bottleneck is API query load, not asset serving or template rendering.
- **Memory overhead:** Embedded assets consume ~2MB of binary size (uncompressed). At runtime, they are served directly from the binary's memory mapping with no additional allocation.

### 9.4 Client-Side Performance

- **No virtual DOM.** HTMX performs surgical DOM replacement using `innerHTML`. For the data volumes in Brioche (tens to hundreds of table rows, not thousands), this is faster than any virtual DOM diffing.
- **uPlot canvas rendering.** uPlot renders directly to a `<canvas>` element, avoiding DOM overhead for chart rendering. It handles 10M+ data points at 60fps.
- **Lazy loading.** Chart data is loaded asynchronously after the page structure renders. Tabs (instance table, logs, deploy history) load their content on first activation, not on page load.

---

## 10. Testing Strategy

### 10.1 API Endpoint Testing

Every Brioche API endpoint is tested at the Rust level using `axum::test` utilities. Tests run against an in-memory cluster state (no real Raft, no real Mayo) with fixture data.

```rust
#[tokio::test]
async fn test_cluster_overview_returns_valid_response() {
    let app = test_app_with_fixtures();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/cluster/summary")
                .header("Authorisation", "Bearer test-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body: ClusterOverview = parse_json(response).await;
    assert_eq!(body.node_count, 12);
    assert!(body.total_cpu_percent >= 0.0 && body.total_cpu_percent <= 100.0);
}

#[tokio::test]
async fn test_app_detail_returns_encrypted_env_values() {
    let app = test_app_with_fixtures();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/apps/web/env")
                .header("Authorisation", "Bearer test-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let env: HashMap<String, EnvValue> = parse_json(response).await;
    // Ensure encrypted values are never exposed
    assert!(matches!(env.get("DATABASE_URL"), Some(EnvValue::Encrypted)));
    // Plain values are visible
    assert!(matches!(env.get("LOG_LEVEL"), Some(EnvValue::Plain(_))));
}

#[tokio::test]
async fn test_write_endpoints_require_csrf_token() {
    let app = test_app_with_fixtures();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/apps/web/rollback")
                .header("Authorisation", "Bearer test-admin-token")
                // No X-CSRF-Token header
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_read_only_token_cannot_trigger_deploy() {
    let app = test_app_with_fixtures();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/apps/web/rollback")
                .header("Authorisation", "Bearer test-readonly-token")
                .header("X-CSRF-Token", "valid-csrf-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
```

### 10.2 Template Rendering Tests

Askama templates are type-checked at compile time, which eliminates an entire class of rendering bugs (missing variables, type mismatches). Additional rendering tests verify that:

- HTML output is well-formed.
- Dynamic content is properly escaped (inject `<script>alert(1)</script>` into app names and verify it appears as escaped text).
- Empty states render correctly (zero apps, zero alerts, zero nodes).
- Large data sets render without excessive DOM size (100 nodes, 50 apps, 1000 events).

```rust
#[test]
fn test_cluster_overview_template_escapes_html() {
    let page = ClusterOverviewPage {
        cluster: ClusterOverview {
            cluster_name: "<script>alert(1)</script>".to_string(),
            // ... other fields
        },
        // ...
    };

    let rendered = page.render().unwrap();
    assert!(!rendered.contains("<script>alert(1)</script>"));
    assert!(rendered.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
}

#[test]
fn test_empty_cluster_renders_gracefully() {
    let page = ClusterOverviewPage {
        cluster: ClusterOverview {
            node_count: 0,
            app_count: 0,
            // ...
        },
        alerts: vec![],
        recent_deploys: vec![],
        // ...
    };

    let rendered = page.render().unwrap();
    assert!(rendered.contains("No apps deployed"));
    assert!(rendered.contains("No active alerts"));
}
```

### 10.3 Accessibility Testing

Brioche targets WCAG 2.1 AA compliance.

**Automated checks (run in CI):**

- `axe-core` via headless browser tests to catch accessibility violations.
- Lighthouse accessibility audit score target: >90.

**Manual verification checklist:**

- All interactive elements are keyboard-navigable (Tab, Enter, Escape).
- Focus indicators are visible on all interactive elements.
- Colour is never the sole indicator of state (status badges include text labels alongside colour).
- All charts have text alternatives (summary tables or `aria-label` descriptions).
- Alert severity uses both colour and icon (critical: red + exclamation, warning: yellow + triangle, info: blue + circle).
- Screen reader navigation: pages use proper heading hierarchy (`h1` > `h2` > `h3`), landmark regions (`<nav>`, `<main>`, `<aside>`), and ARIA labels on dynamic regions.
- Contrast ratios meet 4.5:1 for normal text, 3:1 for large text (both light and dark themes).

### 10.4 Integration Testing

End-to-end tests run a real single-node Reliaburger cluster (Bun with all subsystems), open Brioche in a headless browser (Chromium via `headless_chrome` or Playwright), and verify:

- Login flow works with a valid token.
- Cluster overview page loads and displays correct data.
- Navigating to an app detail page shows correct instance count.
- Streaming logs display new log lines within 2 seconds.
- Triggering a rollback via the UI results in an actual deploy.
- Staleness banner appears when the API is artificially delayed.

These tests are slower (5-10 seconds each) and run in a separate CI step from unit tests.

---

## 11. Prior Art

### 11.1 Kubernetes Dashboard

The Kubernetes Dashboard is a separately deployed web UI (`kubectl apply -f` a YAML manifest). It provides resource listing, YAML editing, and basic metrics visualisation.

**What works:** Comprehensive resource browser; per-resource detail views; namespace filtering.

**What does not work for Reliaburger's goals:**

- Separate installation and lifecycle management. It is yet another component to deploy, version, and secure.
- Requires a metrics-server or Prometheus for any metrics visualisation. Without these, the dashboard shows resource lists only.
- No built-in log streaming (requires separate logging infrastructure).
- Security is complex: it requires its own RBAC, service account, and either a kubectl proxy, a NodePort, or an ingress.
- https://github.com/kubernetes/dashboard

### 11.2 Nomad UI

HashiCorp Nomad includes a built-in web UI served by the Nomad server process. It provides job listing, allocation detail, log streaming, and basic cluster topology.

**What we borrow:**

- Built-in: no separate install. The UI is part of the server binary.
- Clean, focused design: job-centric views rather than generic resource browsers.
- Log streaming built into the UI.
- Topology visualisation showing allocation placement across nodes.

**What we do differently:**

- Nomad's UI is built with Ember.js (~1.5MB of JS). Brioche targets <110KB.
- Nomad has no built-in metrics charts (requires Prometheus + Grafana for time-series). Brioche includes time-series charts from Mayo.
- Nomad has no GitOps view. Brioche shows Lettuce sync status.
- Nomad's UI runs only on server nodes. Brioche runs on every node.
- https://developer.hashicorp.com/nomad/docs/operations/web-ui

### 11.3 Portainer

Portainer is a standalone container management UI that runs as a container itself. It supports Docker, Swarm, and Kubernetes.

**What works:** Simple setup (single container); good Docker beginner experience; environment management.

**What does not work for Reliaburger's goals:**

- It is a separate application with its own state, its own database, and its own user management.
- Designed as a management layer on top of existing orchestrators, not as part of one.
- Heavy: the Portainer container uses 200-500MB of memory.

### 11.4 Grafana

Grafana is the de facto standard for metrics dashboards. It connects to Prometheus (or other data sources) and renders configurable dashboards.

**What works:** Powerful query language; flexible dashboard composition; alerting; plugin ecosystem; used by nearly every production Kubernetes deployment.

**Why Brioche replaces it for the common case:**

- Grafana requires a separate deployment, its own database (SQLite or PostgreSQL), and a connection to a metrics backend.
- Dashboard configuration is a discipline unto itself. Teams spend significant time writing PromQL queries and laying out panels.
- Brioche ships pre-built dashboards that cover 90% of operational needs. Teams that need custom dashboards beyond Brioche's scope can still use Grafana via Mayo's Prometheus-compatible remote-read API.

### 11.5 Consul UI

HashiCorp Consul's built-in UI provides service catalog browsing, health check status, key/value store viewing, and intention (service-to-service ACL) management.

**What we borrow:**

- Service health status is front and center.
- Real-time health check updates.
- Clean service detail pages with health check history.

**What we do differently:**

- Consul's UI is focused on service mesh and service discovery. Brioche covers the full operational scope: deployments, metrics, logs, GitOps, and alerts in addition to service status.

---

## 12. Libraries and Dependencies

### 12.1 Rust Crates (Backend)

| Crate | Version | Purpose | Size Impact |
|-------|---------|---------|-------------|
| `axum` | 0.7+ | HTTP framework (already used by Bun's API server) | 0 (shared) |
| `rust-embed` | 8+ | Static asset embedding at compile time | ~5KB runtime |
| `askama` | 0.12+ | Compile-time HTML templates | 0 runtime (generates code) |
| `tokio-stream` | 0.1+ | SSE stream support | 0 (already a tokio dependency) |
| `mime_guess` | 2+ | Content-Type detection for embedded assets | ~50KB (MIME database) |

**Crates explicitly not used:**

- `actix-web`: Bun already uses `axum`. Adding a second HTTP framework would be wasteful.
- `maud`: Macro-based templating is harder to read for HTML-heavy pages. Askama's file-based templates are more maintainable and allow designers to view templates as HTML files.

### 12.2 Frontend Libraries

| Library | Version | Size (gzipped) | Purpose |
|---------|---------|----------------|---------|
| HTMX | 2.0+ | ~14KB | Dynamic page updates, SSE integration, form handling |
| uPlot | 1.6+ | ~10KB | High-performance time-series charting |

**Libraries explicitly not used:**

- React, Vue, Svelte, Angular: Too heavy. Brioche does not need a component framework; HTMX + server-rendered HTML covers the use case with 10x less JavaScript.
- Leptos, Dioxus: Rust WASM frameworks. While thematically appealing for an all-Rust stack, they produce larger bundles (100-300KB WASM), require a WASM build pipeline, and have less mature ecosystems than HTMX for this use case. Reconsidered if the Rust WASM ecosystem matures further.
- Chart.js: Larger than uPlot (~60KB vs ~10KB) and significantly slower for large time-series datasets. uPlot was purpose-built for time-series.
- D3.js: Too low-level for dashboard charts. Would require significant custom code.
- Tailwind CSS: Requires a build step (PostCSS). Brioche uses hand-written CSS to avoid Node.js in the build pipeline.

### 12.3 Frontend Build Pipeline

There is no frontend build pipeline. The frontend assets are:

1. **HTMX and uPlot:** Vendored as minified JS files in the `brioche/dist/` directory, checked into the repository. Updated manually when upgrading versions.
2. **CSS:** Hand-written, stored in `brioche/dist/styles.css`. No preprocessor.
3. **Custom JS:** Stored in `brioche/dist/brioche.js`. No transpilation, no bundler. Written in plain ES2020+ JavaScript (all target browsers support this).
4. **HTML templates:** Stored in `brioche/templates/` and compiled by Askama into Rust code at `cargo build` time.
5. **Icons:** SVG sprite file, stored in `brioche/dist/icons.svg`.

This means `cargo build` is the only build command. There is no `npm install`, no `npx webpack`, no `yarn build`. Contributors working on the backend never need Node.js installed.

---

## 13. Open Questions

### 13.1 Custom Dashboard Support

**Question:** Should Brioche support user-defined custom dashboards (arbitrary metrics queries, custom layouts)?

**Arguments for:**

- Covers the long tail of team-specific monitoring needs.
- Reduces the "but I still need Grafana for X" escape hatch.

**Arguments against:**

- Dramatically increases UI complexity (dashboard editor, layout engine, query builder).
- Grafana already solves this problem and can connect to Mayo via Prometheus remote-read.
- Scope creep risk: Brioche should be a focused operational dashboard, not a general-purpose visualisation tool.

**Current leaning:** No. Brioche provides fixed, opinionated dashboards. Teams needing custom dashboards use Grafana (or similar) connected to Mayo's Prometheus-compatible API. Revisit if user feedback consistently requests specific additional views.

### 13.2 Dark Mode

**Question:** Should Brioche support a dark theme?

**Current leaning:** Yes, as a toggle stored in a browser cookie. Most operational dashboards are viewed in dim server rooms or during late-night incidents. Implementation is straightforward with CSS custom properties:

```css
:root {
    --bg-primary: #ffffff;
    --text-primary: #1a1a2e;
    --status-healthy: #10b981;
    --status-degraded: #f59e0b;
    --status-error: #ef4444;
}

[data-theme="dark"] {
    --bg-primary: #0f0f1a;
    --text-primary: #e2e8f0;
    --status-healthy: #34d399;
    --status-degraded: #fbbf24;
    --status-error: #f87171;
}
```

**Decision needed:** Ship with dark mode from day one, or add it post-launch?

### 13.3 Mobile Responsiveness

**Question:** Should Brioche be fully responsive for mobile/tablet viewing?

**Current leaning:** Tablet support (yes), phone support (limited). Operators typically use Brioche from laptops or desktops. Tablet support is useful for on-call engineers checking status from a personal device. Full phone optimisation (collapsing tables, reorganizing charts) is significant work for a rare use case.

**Minimum commitment:** Pages should not break on narrow viewports. Tables should horizontally scroll. Charts should resize. But optimised mobile layouts are not a priority.

### 13.4 WebSocket vs SSE for Real-Time Updates

**Question:** Use WebSocket or Server-Sent Events for real-time data streaming?

**Current leaning:** SSE.

| Factor | WebSocket | SSE |
|--------|-----------|-----|
| Direction | Bidirectional | Server to client |
| Reconnection | Manual | Automatic (built into EventSource) |
| HTTP/2 multiplexing | Separate connection | Multiplexed on existing connection |
| Proxy compatibility | Can be problematic | Works with any HTTP proxy |
| HTMX integration | Requires extension | Native support (`hx-ext="sse"`) |
| Use case fit | Chat, gaming, bidirectional | Log streaming, event feeds, dashboards |

Brioche's real-time data is exclusively server-to-client (logs, events, deploy progress). SSE is the simpler, more compatible choice. WebSocket would only be needed if Brioche added interactive features requiring bidirectional communication (e.g., a terminal emulator), which is not planned (the Relish TUI serves that purpose).

**Decision needed:** Confirm SSE as the sole real-time mechanism, or keep WebSocket as an option for future use cases.

### 13.5 Offline / PWA Support

**Question:** Should Brioche be installable as a Progressive Web App with offline caching?

**Current leaning:** No. Brioche is a real-time operational dashboard. Offline data is stale data, and stale data in an operational dashboard is dangerous. The staleness banner (Section 7.1) already handles the case where fresh data is unavailable. A service worker that aggressively caches API responses could mask failures and lead operators to make decisions based on outdated information.

### 13.6 Multi-Cluster Support

**Question:** Should a single Brioche instance aggregate data from multiple Reliaburger clusters?

**Current leaning:** Not in v1. Each Brioche instance shows the cluster it is running in. Multi-cluster visibility can be achieved by opening multiple browser tabs (one per cluster) or by building an external aggregation layer that queries multiple clusters' APIs. This is a deliberate scope constraint to keep Brioche simple.

### 13.7 Embeddable Components

**Question:** Should Brioche's chart components be embeddable in external pages (e.g., team status pages, internal wikis)?

**Current leaning:** Deferred. The `frame-ancestors 'none'` CSP header currently prevents embedding. If there is demand, specific chart endpoints could be served with relaxed CSP and an `X-Frame-Options: SAMEORIGIN` or `ALLOW-FROM` directive.

---

## Appendix: API Endpoint Summary

All endpoints are prefixed with `/api/v1`. Authentication is required for all endpoints.

### Read Endpoints (GET)

| Endpoint | Returns | Used By |
|----------|---------|---------|
| `/cluster/summary` | `ClusterOverview` | Cluster overview page |
| `/nodes` | `Vec<NodeSummary>` | Cluster overview node map, node list |
| `/nodes/:name` | `NodeDetail` | Node detail page |
| `/apps` | `Vec<AppSummary>` | App list, cluster overview |
| `/apps/:name` | `AppDetail` | App detail page |
| `/apps/:name/env` | `HashMap<String, EnvValue>` | App detail environment tab |
| `/apps/:name/instances` | `Vec<AppInstance>` | App detail instance table |
| `/deploys` | `Vec<DeployEvent>` | Cluster overview, app detail |
| `/deploys/:id` | `DeployEvent` | Deploy detail |
| `/alerts` | `Vec<AlertSummary>` | Alert dashboard |
| `/alerts/history` | `Vec<AlertSummary>` | Alert history |
| `/ingress/routes` | `Vec<IngressRoute>` | Ingress overview |
| `/gitops/status` | `GitOpsStatus` | GitOps page |
| `/gitops/history` | `Vec<GitOpsSyncEvent>` | GitOps sync history |
| `/jobs` | `JobOverview` | Jobs view |
| `/jobs/:id` | `JobSummary` | Job detail |
| `/events` | `Vec<ClusterEvent>` | Event stream (paginated) |
| `/metrics/query` | `MetricsQueryResponse` | All chart data |

### Streaming Endpoints (SSE)

| Endpoint | Stream Content | Used By |
|----------|---------------|---------|
| `/logs/:app/stream` | Log lines (HTML fragments) | App detail log tail |
| `/events/stream` | Cluster events | Cluster overview event stream |
| `/deploys/:id/stream` | Deploy progress updates | Deploy progress indicator |

### Write Endpoints (POST/PUT/DELETE)

| Endpoint | Action | Required Role |
|----------|--------|---------------|
| `POST /apps/:name/rollback` | Trigger rollback to specified version | deployer |
| `POST /apps/:name/scale` | Change replica count | deployer |
| `POST /apps/:name/restart` | Rolling restart of all instances | deployer |
| `POST /gitops/sync` | Trigger immediate GitOps sync | admin |
| `POST /jobs/:id/retry` | Retry a failed job | deployer |
| `POST /alerts/:id/acknowledge` | Acknowledge an alert | deployer |
| `POST /alerts/:id/suppress` | Suppress an alert | admin |
| `POST /auth/validate` | Validate a token and create session | (unauthenticated) |
| `POST /auth/logout` | Clear session cookie | any |

### UI Fragment Endpoints (GET, returns HTML)

These endpoints return pre-rendered HTML fragments for HTMX partial page updates, avoiding a full JSON round-trip and client-side rendering.

| Endpoint | Returns | Used By |
|----------|---------|---------|
| `/ui/cluster-overview` | Cluster overview HTML fragment | HTMX polling on overview page |
| `/ui/alerts-summary` | Active alerts HTML fragment | HTMX polling on overview page |
| `/ui/apps/:name/instances` | Instance table HTML fragment | HTMX polling on app detail page |
| `/ui/nodes/:name/resources` | Resource summary HTML fragment | HTMX polling on node detail page |
