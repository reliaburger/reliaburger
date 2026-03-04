# Mayo: Embedded Metrics and Time-Series Database

**Component:** Mayo (Metrics TSDB + Built-In Alerts)
**Binary:** Embedded in Bun (the single-node agent)
**Status:** Design
**Whitepaper Reference:** Section 15

---

## 1. Overview

Mayo is Reliaburger's embedded, per-node time-series database (TSDB). It provides a complete metrics pipeline -- collection, storage, querying, dashboarding, and alerting -- with zero configuration required. Every Bun instance runs a Mayo TSDB that:

- **Auto-collects** infrastructure metrics (CPU, memory, network, disk, GPU) for every app and the node itself.
- **Scrapes Prometheus-format metrics** from application `/metrics` endpoints, auto-detected at startup.
- **Stores data locally** in a 3-tier retention scheme (full-resolution, downsampled, archived) requiring approximately 50-100MB/day per busy node.
- **Participates in hierarchical aggregation** via council members, enabling cluster-wide queries at 10,000 nodes without scatter-gathering every node.
- **Exposes a Prometheus-compatible remote-read API** so teams can federate data into external Prometheus/Thanos/Cortex/Grafana stacks.
- **Evaluates built-in and custom alert rules** using a PromQL-compatible expression subset, with webhook-based notification and per-app suppression/tuning.

The result is that operators get Prometheus + Thanos + Alertmanager functionality out of the box, embedded in the single Bun binary, with no external dependencies, no central database, and no configuration for the common case.

---

## 2. Dependencies

### Internal Components

| Component | Relationship |
|-----------|-------------|
| **Bun** (node agent) | Host process. Bun's container runtime (Grill) provides cgroup stats for auto-collected metrics. Bun's Prometheus scraper feeds application metrics into the local Mayo TSDB. Bun drives Mayo's lifecycle (init, retention, shutdown). |
| **Council** (Raft consensus group) | Council members act as the hierarchical aggregation tier. Each council member receives pre-aggregated rollups from its assigned subset of cluster nodes. Cluster-wide queries fan out to council aggregators (3-7 nodes) rather than every node. |
| **Brioche** (web UI) | Queries Mayo via internal gRPC for dashboard rendering. Brioche displays cluster overview, app detail, node detail, and ingress dashboards -- all sourced from Mayo data. Alert state is also surfaced through Brioche. |
| **Relish** (CLI) | `relish metrics`, `relish alerts`, and `relish top` query Mayo. The CLI can target a single node's TSDB or fan out through the leader for cross-node queries. |
| **Mustard** (gossip) | Provides cluster membership and node identity, used by Mayo to determine which nodes are alive for query fan-out and which council member a node reports rollups to. |
| **Patty** (scheduler) | Provides the app-to-node placement map so the leader knows which nodes to fan out to for single-app queries. |
| **Wrapper** (ingress proxy) | Emits per-route ingress metrics (request count, latency percentiles, error rates) that Mayo collects. |

### External Interfaces

| Interface | Direction | Description |
|-----------|-----------|-------------|
| Prometheus remote-read API | Inbound | External Prometheus/Thanos/Cortex instances can query Mayo data via the standard `remote_read` protocol. |
| Prometheus scrape endpoint | Inbound | External Prometheus instances can scrape `/metrics` on each node to pull Mayo-collected data. |
| Application `/metrics` | Outbound (node-local) | Bun scrapes each app instance's Prometheus exposition endpoint. |
| Webhook notifications | Outbound | Alert notifications dispatched to Slack, PagerDuty, or arbitrary HTTP endpoints. |

---

## 3. Architecture

### 3.1 Per-Node Storage (No Central Database)

Each node stores its own metrics locally. There is no central metrics database. This design means:

- Metrics storage scales linearly with cluster size.
- There is no single bottleneck or point of failure for metrics.
- A node failure loses only that node's local historical data (rollups on council aggregators provide cluster-level continuity).
- No inter-node replication of raw metric data is required.

```
┌──────────────────────────────────────────────────────────────────┐
│  Node (Bun)                                                      │
│                                                                  │
│  ┌──────────────┐    ┌──────────────┐    ┌───────────────────┐  │
│  │ Auto-Collect  │    │ Prom Scraper │    │ Wrapper Ingress   │  │
│  │ (cgroups,     │    │ (/metrics    │    │ (request count,   │  │
│  │  /proc, GPU)  │    │  endpoints)  │    │  latency, errors) │  │
│  └──────┬───────┘    └──────┬───────┘    └────────┬──────────┘  │
│         │                   │                     │              │
│         └───────────────────┼─────────────────────┘              │
│                             ▼                                    │
│                   ┌─────────────────┐                            │
│                   │   Mayo TSDB     │                            │
│                   │                 │                            │
│                   │  ┌───────────┐  │                            │
│                   │  │ Full Tier │  │  10s intervals, 24h        │
│                   │  ├───────────┤  │                            │
│                   │  │ Downsamp. │  │  1min aggregates, 7d       │
│                   │  ├───────────┤  │                            │
│                   │  │ Archived  │  │  1h aggregates, 90d        │
│                   │  └───────────┘  │                            │
│                   └────────┬────────┘                            │
│                            │                                     │
│              ┌─────────────┼────────────┐                        │
│              ▼             ▼            ▼                         │
│     Local queries   Rollup push   Remote-read API                │
│     (Brioche/CLI)   to council    (ext. Prometheus)              │
│                     aggregator                                   │
└──────────────────────────────────────────────────────────────────┘
```

### 3.2 Three-Tier Retention

Data is stored at three resolution tiers with configurable retention:

| Tier | Resolution | Default Retention | Purpose |
|------|-----------|-------------------|---------|
| **Full** | 10-second intervals | 24 hours | High-resolution debugging, recent incident investigation |
| **Downsampled** | 1-minute aggregates (min, max, sum, count, avg) | 7 days | Week-over-week comparisons, capacity trends |
| **Archived** | 1-hour aggregates (min, max, sum, count, avg) | 90 days | Long-term trends, quarterly reviews |

Tier transitions are performed by a background compaction task on each node. The downsampler runs every minute, aggregating the previous minute's full-resolution samples. The archiver runs every hour, aggregating the previous hour's downsampled data. Both tasks operate on immutable, closed time windows to avoid consistency issues.

### 3.3 Hierarchical Aggregation via Council Members

Cluster-wide queries use a hierarchical aggregation model designed to scale to 10,000 nodes. Each council member acts as an aggregator for a subset of cluster nodes (~1,400-2,000 nodes per council member in a 10,000-node cluster with a 5-7 member council).

```
Cluster-wide query path:
  Brioche UI → Leader → Council aggregators (5 nodes)
                         ↑ each holds rollups from ~2,000 nodes
                         → merged result in <500ms

Single-app query path:
  Brioche UI → Leader → Nodes running that app (3-10 nodes)
                         → merged result in <50ms
```

**Rollup push:** Each node periodically (every 60 seconds) pushes pre-aggregated rollups -- 1-minute summaries of top-level metrics (per-app CPU, memory, network; per-node totals) -- to its assigned council aggregator. The assignment is deterministic based on consistent hashing of the node ID against council member IDs, and rebalances automatically when council membership changes.

**Council aggregator storage:** Council members store received rollups in a separate Mayo instance (or logically separated partition) with their own retention policy (default: 24 hours of 1-minute rollups, 30 days of 1-hour rollups). This data is used exclusively for cluster-wide queries.

**Query fan-out for single-app queries:** The leader (or any council member handling the query) consults the Patty placement map to determine which nodes are running the target app, then fans the query out only to those nodes (typically 3-10). Each node evaluates the query against its local Mayo TSDB and returns results. The querying node merges the partial results.

**Query fan-out for cluster-wide queries:** The querying node fans out to all council aggregators (3-7 nodes), each of which evaluates the query against its aggregated rollup data and returns results. The querying node merges the partial results. This bounds fan-out to the council size regardless of cluster size.

### 3.4 Prometheus-Compatible Remote-Read API

Each node exposes a Prometheus-compatible remote-read API endpoint at `/api/v1/read`. External Prometheus instances can configure Mayo as a `remote_read` target to federate data out for teams that prefer their existing Prometheus + Grafana stack. The API supports:

- The standard Prometheus remote-read protobuf protocol.
- Label matchers (`=`, `!=`, `=~`, `!~`).
- Time range selection.
- Streaming chunked responses for large result sets.

Additionally, each node exposes its collected metrics via a standard `/metrics` endpoint in Prometheus exposition format, allowing external Prometheus instances to scrape Mayo directly.

---

## 4. Data Structures

### 4.1 Core Metric Types

```rust
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

/// A unique metric identity, combining name and label set.
/// Labels are sorted by key for deterministic hashing and comparison.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MetricDescriptor {
    /// Metric name, e.g. "cpu_usage_seconds_total", "http_requests_total"
    pub name: String,
    /// Sorted label key-value pairs. Always includes at minimum:
    /// - "node" (originating node ID)
    /// - "app" (app name, if app-scoped)
    /// - "instance" (app instance ID, if instance-scoped)
    pub labels: BTreeMap<String, String>,
    /// Metric type for exposition and query semantics
    pub metric_type: MetricType,
    /// Human-readable description (from HELP line in Prometheus exposition)
    pub help: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetricType {
    Counter,
    Gauge,
    Histogram,
    Summary,
    Untyped,
}

/// A single data point: timestamp + float64 value.
/// Timestamps are milliseconds since Unix epoch.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub timestamp_ms: i64,
    pub value: f64,
}

/// A time series is a metric descriptor plus an ordered sequence of samples.
/// In memory, samples are stored in a compressed chunk (see ChunkEncoding).
/// The descriptor is interned via MetricId to avoid repeated allocation.
#[derive(Debug)]
pub struct TimeSeries {
    pub id: MetricId,
    pub descriptor: MetricDescriptor,
    pub samples: Vec<Sample>,
}

/// Compact numeric identifier for a metric descriptor within a single
/// Mayo TSDB instance. Assigned at ingestion time, used for all internal
/// lookups. The mapping (MetricId <-> MetricDescriptor) is stored in the
/// label index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MetricId(pub u64);
```

### 4.2 Retention Tiers and Downsampled Aggregates

```rust
/// Defines a single retention tier's parameters.
#[derive(Debug, Clone)]
pub struct RetentionTier {
    pub name: TierName,
    /// Resolution of data in this tier
    pub resolution: Duration,
    /// How long data is kept before deletion
    pub retention: Duration,
    /// Compression algorithm for on-disk blocks
    pub compression: Compression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierName {
    Full,
    Downsampled,
    Archived,
}

#[derive(Debug, Clone, Copy)]
pub enum Compression {
    /// No compression (used for the in-memory head chunk)
    None,
    /// Gorilla-style XOR encoding for timestamps and values (full tier on-disk)
    Gorilla,
    /// Zstd compression over Gorilla-encoded blocks (downsampled + archived tiers)
    GorillaPlusZstd { level: i32 },
}

/// An aggregated data point produced by the downsampler or archiver.
/// Captures the statistical summary of all raw samples within the
/// aggregation window.
#[derive(Debug, Clone, Copy)]
pub struct AggregationRollup {
    /// Start of the aggregation window (inclusive)
    pub window_start_ms: i64,
    /// End of the aggregation window (exclusive)
    pub window_end_ms: i64,
    /// Minimum sample value in the window
    pub min: f64,
    /// Maximum sample value in the window
    pub max: f64,
    /// Sum of all sample values (for computing averages)
    pub sum: f64,
    /// Number of samples aggregated
    pub count: u32,
    /// For counters: total increase over the window (handles resets)
    pub counter_increase: Option<f64>,
}

/// Pre-aggregated rollup pushed from worker nodes to council aggregators.
/// One of these is generated per metric per node per push interval (60s).
#[derive(Debug, Clone)]
pub struct NodeRollup {
    /// Node that produced this rollup
    pub node_id: String,
    /// Timestamp of rollup generation
    pub timestamp_ms: i64,
    /// Per-metric aggregation summaries for the last push interval
    pub metrics: Vec<RollupEntry>,
}

#[derive(Debug, Clone)]
pub struct RollupEntry {
    pub descriptor: MetricDescriptor,
    pub rollup: AggregationRollup,
}
```

### 4.3 Alert Data Structures

```rust
/// A single alert rule definition. Can be a built-in default or custom.
#[derive(Debug, Clone)]
pub struct AlertRule {
    /// Unique name, e.g. "cpu.throttled", "api.error_rate_high"
    pub name: String,
    /// PromQL-compatible expression that evaluates to a boolean or instant vector.
    /// When the expression returns a non-empty result set (or a scalar > 0),
    /// the alert is considered firing.
    pub expr: String,
    /// Duration the expression must continuously evaluate to true before
    /// the alert transitions from Pending to Firing.
    pub for_duration: Duration,
    /// Alert severity level
    pub severity: AlertSeverity,
    /// Human-readable message template. Supports label interpolation:
    /// "{{ $labels.app }} CPU throttled at {{ $value }}%"
    pub message: String,
    /// Whether this is a built-in default alert (can be suppressed per-app)
    pub is_builtin: bool,
    /// Per-app overrides: suppression or threshold tuning
    pub app_overrides: BTreeMap<String, AlertOverride>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertSeverity {
    Info,
    Warning,
    Critical,
}

/// Per-app override for a built-in alert rule.
#[derive(Debug, Clone)]
pub enum AlertOverride {
    /// Completely suppress this alert for the app
    Suppressed,
    /// Override the threshold value. The meaning depends on the alert:
    /// for memory.low, this overrides the percentage threshold.
    Tuned { threshold: f64 },
}

/// Runtime state of an alert rule evaluation.
#[derive(Debug)]
pub struct AlertState {
    pub rule: AlertRule,
    pub status: AlertStatus,
    /// When the expression first started evaluating to true (for `for` duration)
    pub pending_since: Option<SystemTime>,
    /// When the alert transitioned to Firing
    pub firing_since: Option<SystemTime>,
    /// Labels from the expression result (identifies the specific app/node)
    pub active_labels: BTreeMap<String, String>,
    /// Last evaluation timestamp
    pub last_eval: SystemTime,
    /// Last evaluation error, if any
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertStatus {
    /// Expression is not true; alert is inactive
    Inactive,
    /// Expression is true but `for` duration has not elapsed
    Pending,
    /// Expression has been true for >= `for` duration; notifications sent
    Firing,
}

/// Configuration for an alert notification destination.
#[derive(Debug, Clone)]
pub struct AlertNotificationDestination {
    pub destination_type: NotificationType,
    pub url: String,
    /// Which severities to send to this destination
    pub severity_filter: Vec<AlertSeverity>,
    /// Optional shared secret for HMAC signing of webhook payloads
    pub secret: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum NotificationType {
    Webhook,
}
```

### 4.4 On-Disk Format

Mayo's on-disk format is inspired by Prometheus TSDB's block-based design but simplified for the embedded use case.

```
<mayo_data_dir>/
├── head/                          # In-memory + WAL for current data
│   ├── wal/                       # Write-ahead log for crash recovery
│   │   ├── 000001                 # WAL segment (append-only, 128MB max)
│   │   ├── 000002
│   │   └── ...
│   └── chunks_head/               # Memory-mapped chunks for recent data
│       ├── 000001
│       └── ...
├── blocks/                        # Immutable, compacted blocks
│   ├── 01HQXYZ.../               # Block ID (ULID)
│   │   ├── meta.json             # Block metadata (time range, stats)
│   │   ├── index                 # Label index (posting lists, label pairs)
│   │   ├── chunks/               # Time-series data chunks
│   │   │   ├── 000001            # Gorilla-encoded chunk file
│   │   │   └── ...
│   │   └── tombstones            # Deletion markers (for retention pruning)
│   └── ...
├── downsampled/                   # 1-minute aggregate blocks
│   ├── 01HQXYZ.../
│   │   ├── meta.json
│   │   ├── index
│   │   └── chunks/
│   │       └── ...
│   └── ...
├── archived/                      # 1-hour aggregate blocks
│   ├── 01HQXYZ.../
│   │   ├── meta.json
│   │   ├── index
│   │   └── chunks/               # Zstd-compressed chunks
│   │       └── ...
│   └── ...
└── lock                           # Process-level file lock
```

**Block structure:** Each block covers a fixed time window (2 hours for full-resolution, 24 hours for downsampled, 7 days for archived). Blocks are immutable once written. The `meta.json` file contains:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockMeta {
    /// Unique block identifier (ULID, lexicographically sortable by time)
    pub ulid: String,
    /// Minimum timestamp (inclusive) of any sample in the block
    pub min_time_ms: i64,
    /// Maximum timestamp (exclusive) of any sample in the block
    pub max_time_ms: i64,
    /// Retention tier
    pub tier: TierName,
    /// Number of distinct time series in this block
    pub num_series: u64,
    /// Total number of samples (or rollups) in this block
    pub num_samples: u64,
    /// Block size on disk in bytes
    pub size_bytes: u64,
    /// Compaction level (0 = first write, increments on merges)
    pub compaction_level: u32,
    /// Compression format used for chunks in this block
    pub compression: Compression,
}
```

**Chunk encoding:** Samples within a chunk use Gorilla-style encoding (Facebook's "Gorilla: A Fast, Scalable, In-Memory Time Series Database" paper). Timestamps are delta-of-delta encoded; values are XOR-encoded. This achieves approximately 1.37 bytes per sample for typical infrastructure metrics (compared to 16 bytes raw). Downsampled and archived tiers additionally wrap chunks in zstd compression (level 3 default) for further 2-3x reduction.

**Index structure:** The label index uses posting lists (sorted lists of MetricId for each label value) with intersection/union operations for multi-label lookups. This mirrors the Prometheus TSDB index design. The index is memory-mapped at query time for fast access without loading entirely into RAM.

---

## 5. Operations

### 5.1 Auto-Collection

Bun's metrics collector gathers infrastructure metrics at the configured collection interval (default: 10 seconds) without any user configuration.

**Per-app metrics** (collected via cgroup stats from the Grill container runtime):

| Metric | Source | Type |
|--------|--------|------|
| `app_cpu_usage_seconds_total` | `cpuacct.usage` cgroup | Counter |
| `app_cpu_throttled_seconds_total` | `cpu.stat` (throttled_time) | Counter |
| `app_memory_usage_bytes` | `memory.current` cgroup | Gauge |
| `app_memory_limit_bytes` | `memory.max` cgroup | Gauge |
| `app_oom_kills_total` | `memory.events` (oom_kill) | Counter |
| `app_network_receive_bytes_total` | `/proc/[pid]/net/dev` | Counter |
| `app_network_transmit_bytes_total` | `/proc/[pid]/net/dev` | Counter |
| `app_restarts_total` | Bun restart tracker | Counter |
| `app_gpu_utilization_percent` | NVML / `nvidia-smi` equivalent | Gauge |
| `app_gpu_vram_usage_bytes` | NVML | Gauge |

GPU metrics are only collected for apps with a GPU allocation. Apps without GPU allocation do not incur any GPU metric collection overhead.

**Per-node metrics** (collected from `/proc`, `/sys`, and device interfaces):

| Metric | Source | Type |
|--------|--------|------|
| `node_cpu_usage_percent` | `/proc/stat` | Gauge |
| `node_memory_total_bytes` | `/proc/meminfo` | Gauge |
| `node_memory_available_bytes` | `/proc/meminfo` | Gauge |
| `node_disk_usage_bytes{mount=...}` | `statvfs` per mount | Gauge |
| `node_disk_total_bytes{mount=...}` | `statvfs` per mount | Gauge |
| `node_network_receive_bytes_total{device=...}` | `/proc/net/dev` | Counter |
| `node_network_transmit_bytes_total{device=...}` | `/proc/net/dev` | Counter |
| `node_running_apps` | Bun app tracker | Gauge |
| `node_gpu_utilization_percent{device=...}` | NVML | Gauge |
| `node_gpu_temperature_celsius{device=...}` | NVML | Gauge |

**Per-ingress-route metrics** (emitted by the Wrapper ingress proxy):

| Metric | Source | Type |
|--------|--------|------|
| `ingress_requests_total{route=..., status_code=...}` | Wrapper | Counter |
| `ingress_request_duration_seconds{route=..., quantile=...}` | Wrapper | Summary |
| `ingress_http_errors_total{route=..., class="4xx"\|"5xx"}` | Wrapper | Counter |

All auto-collected metrics carry default labels: `node` (node ID), `app` (app name), `instance` (instance ID). Ingress metrics additionally carry `route` and `status_code` labels.

### 5.2 Prometheus Scraping

Metrics scraping is enabled **by default** for every app that declares a `port`. No configuration is required for the standard case.

**Discovery and probing flow:**

1. When Bun starts an app instance (or on Bun startup for existing instances), the scraper sends an HTTP GET to `http://127.0.0.1:{app_host_port}/metrics`.
2. If the response is HTTP 200 with `Content-Type: text/plain` and contains valid Prometheus exposition format markers (`# HELP` / `# TYPE` lines), scraping is enabled for that instance at the configured interval (default: 30 seconds).
3. If the endpoint returns a non-2xx status, is not present (connection refused / 404), or returns a body that does not parse as Prometheus exposition format, scraping is **silently disabled** for that instance. No error is logged; no configuration is required.
4. Apps without a `port` declaration are never probed.

**Configurable overrides:**

```toml
# Default behaviour (implicit, no config needed):
# metrics = "/metrics"
# metrics_interval = "30s"

# Override the path and/or interval:
[app.api]
image = "myapp:v1.4.2"
port = 8080
metrics = "/prom/metrics"       # non-standard path
metrics_interval = "10s"        # faster scrape for this app

# Disable scraping explicitly:
[app.static-assets]
image = "nginx:alpine"
port = 80
metrics = false
```

**Scrape lifecycle:**

- Scraping is re-probed on app restart (the endpoint may have appeared or disappeared).
- If a scrape times out (default: 5 seconds, configurable), that scrape is skipped and the next attempt proceeds at the normal interval. Three consecutive timeouts log a warning-level message.
- Scraped metrics are merged into the same Mayo TSDB as auto-collected metrics. They are distinguishable by the absence of the `__auto__` internal label.
- Metric names from application scrapes are stored as-is (no prefixing). Label collisions with auto-collected metrics are avoided because auto-collected metrics use the `app_`, `node_`, and `ingress_` prefixes.

### 5.3 Downsampling

A background compaction task runs on each node to produce lower-resolution tiers.

**Full -> Downsampled (every 60 seconds):**

1. Wait until the current minute boundary passes (e.g., at 14:32:00, process the window 14:31:00-14:32:00).
2. For every active time series, read all full-resolution samples in the closed window.
3. Compute the `AggregationRollup` (min, max, sum, count; and `counter_increase` for counters).
4. Append the rollup to the downsampled tier.
5. The full-resolution data is retained until its retention expires (24 hours); downsampling does not delete it.

**Downsampled -> Archived (every 60 minutes):**

1. Same pattern: at the top of each hour, process the previous hour's downsampled rollups.
2. Aggregate the 60 one-minute rollups into a single one-hour rollup.
3. Append to the archived tier.

**Retention pruning:** A background task runs every 15 minutes and deletes blocks whose `max_time_ms` is older than the tier's retention period. Deletion is block-level (entire blocks are removed), not sample-level, so pruning is O(number of blocks) not O(number of samples).

### 5.4 Hierarchical Aggregation

**Node-to-council rollup push:**

Every 60 seconds, each node generates a `NodeRollup` containing 1-minute aggregation summaries for a curated set of top-level metrics:

- Per-app: CPU usage, memory usage, network bytes, restart count
- Per-node: total CPU, memory, disk, network, running app count
- Per-ingress-route: request count, error count, p50/p95/p99 latency

The node pushes this rollup to its assigned council aggregator via an internal gRPC call. The assignment is `hash(node_id) % len(council_members)`, recomputed when council membership changes (Mustard gossip propagates council membership).

**Council aggregator processing:**

The council aggregator stores received `NodeRollup` entries in a separate rollup store. When a cluster-wide query arrives, the aggregator evaluates the query against its local rollup data. For aggregation functions (sum, avg, max), the aggregator can compute partial results from the rollups without needing raw data.

**Aggregator reassignment on council membership change:**

When a council member joins or leaves, nodes recompute their aggregator assignment. The new aggregator will have no historical rollups for its newly assigned nodes. To handle this gracefully:

1. Nodes include their previous 5 minutes of rollups in the first push to a new aggregator.
2. The query layer treats a gap in aggregator rollup data as "data unavailable" and annotates the response, rather than returning zeros.

### 5.5 Query Fan-Out

**Single-app query** (e.g., "show CPU for app.web"):

1. Query arrives at any council member or the leader.
2. The query handler looks up the Patty placement map to find which nodes are running `app.web` (e.g., nodes 3, 7, 11).
3. Query is fanned out in parallel to those 3 nodes.
4. Each node evaluates the PromQL expression against its local Mayo TSDB.
5. Results are merged (series concatenation, deduplication by labels+timestamp).
6. Merged result returned to the caller.

**Cluster-wide query** (e.g., "top 10 apps by CPU" or "total cluster memory usage"):

1. Query arrives at any council member or the leader.
2. Query is fanned out in parallel to all council aggregators (3-7 nodes).
3. Each aggregator evaluates the query against its rollup data.
4. Results are merged and the top-N / aggregation is computed on the merged set.
5. Merged result returned to the caller.

**Query timeout:** Default 10 seconds. If a node does not respond within the timeout, its results are omitted and the response includes a warning annotation listing the unresponsive nodes.

### 5.6 Alert Evaluation

Alert rules are evaluated on the leader node (or, for per-app alerts, on each node locally for its own apps). The evaluation loop runs every 15 seconds (configurable).

**Built-in default alerts (active out of the box):**

| Alert | Condition | Default Threshold | Severity |
|-------|-----------|-------------------|----------|
| `cpu.throttled` | App is consistently CPU-throttled | > 25% of CPU time throttled for 5 minutes | warning |
| `cpu.idle` | App is using far less CPU than allocated | < 10% of allocated CPU for 1 hour | info |
| `oom.kill` | Container killed due to insufficient memory | Any OOM kill event | critical |
| `memory.low` | App is approaching its memory limit | > 85% of memory limit for 5 minutes | warning |
| `disk.filling` | Node filesystem is running out of space | > 80% used (warning), > 90% used (critical) | warning / critical |

These fire automatically for every app and every node with no configuration required.

**Custom alert evaluation:**

Custom alert expressions use a PromQL-compatible subset. The evaluation loop:

1. Parse the expression into an AST (done once at rule load, cached).
2. Evaluate the expression against the local Mayo TSDB (or, for cluster-scoped alerts, via the query fan-out path).
3. If the expression returns a non-empty instant vector (or a scalar > 0), the alert enters `Pending` state and records `pending_since`.
4. If the expression continues to evaluate to true for the `for` duration, the alert transitions to `Firing` and notifications are dispatched.
5. If the expression evaluates to false at any point during the `for` window, the alert resets to `Inactive`.

**PromQL-compatible subset supported:**

- Instant vector selectors: `metric_name{label="value"}`
- Range vector selectors: `metric_name{label="value"}[5m]`
- Aggregation operators: `sum`, `avg`, `min`, `max`, `count`, `topk`, `bottomk`
- Aggregation clauses: `by`, `without`
- Functions: `rate()`, `irate()`, `increase()`, `delta()`, `deriv()`, `abs()`, `ceil()`, `floor()`, `round()`, `clamp()`, `clamp_min()`, `clamp_max()`, `histogram_quantile()`
- Binary operators: `+`, `-`, `*`, `/`, `%`, `^`, `==`, `!=`, `>`, `<`, `>=`, `<=`
- Logical operators: `and`, `or`, `unless`
- Label matchers: `=`, `!=`, `=~`, `!~`

### 5.7 Alert Notification

When an alert transitions to `Firing`, notifications are dispatched to configured destinations.

**Webhook payload format:**

```json
{
  "version": "1",
  "alert": {
    "name": "cpu.throttled",
    "severity": "warning",
    "status": "firing",
    "message": "app.web CPU throttled at 32% for 5 minutes",
    "labels": {
      "app": "web",
      "node": "node-07",
      "instance": "web-3"
    },
    "value": 0.32,
    "started_at": "2026-02-16T14:30:15Z",
    "fired_at": "2026-02-16T14:35:15Z"
  },
  "cluster": "prod",
  "timestamp": "2026-02-16T14:35:15Z"
}
```

A corresponding `resolved` payload is sent when the alert transitions from `Firing` to `Inactive`.

**Configuration:**

```toml
# Simple single-destination:
[alerts.notify]
webhook = "https://hooks.slack.com/services/T.../B.../xxx"

# Multiple destinations with severity filtering:
[[alerts.notify.destinations]]
type = "webhook"
url = "https://hooks.slack.com/services/T.../B.../xxx"
severity = ["critical", "warning"]

[[alerts.notify.destinations]]
type = "webhook"
url = "https://events.pagerduty.com/v2/enqueue"
severity = ["critical"]
```

**Retry policy:** Failed webhook deliveries are retried 3 times with exponential backoff (1s, 5s, 25s). After 3 failures, the notification is dropped and a `mayo_alert_notification_failed_total` metric is incremented.

### 5.8 Alert Suppression and Tuning Per-App

Built-in default alerts can be suppressed or tuned on a per-app basis:

```toml
[app.batch-worker]
image = "worker:latest"
cpu = "100m-2000m"

[app.batch-worker.alerts]
cpu.idle = false                        # suppress idle alerts (intentionally bursty)
memory.low = { threshold = 95 }         # this app is expected to use most of its RAM
```

When `cpu.idle = false`, the alert evaluation loop skips the `cpu.idle` rule for all instances of `app.batch-worker`. When a threshold override is provided, the built-in expression is rewritten with the overridden value before evaluation.

Custom alerts (defined via `[[alert]]`) cannot be suppressed per-app via this mechanism -- they are cluster-wide rules. To scope a custom alert to specific apps, use label matchers in the expression (e.g., `{app="web"}`).

---

## 6. Configuration

All Mayo configuration is optional. The system operates with sensible defaults out of the box.

### 6.1 Collection Configuration

```toml
[mayo]
# How often auto-collected metrics are sampled (default: 10s)
collection_interval = "10s"

# Maximum disk space Mayo may use on this node (default: auto, 5% of disk)
max_storage = "2Gi"

# Retention tier configuration
[mayo.retention]
full = "24h"           # Full-resolution (10s intervals)
downsampled = "7d"     # 1-minute aggregates
archived = "90d"       # 1-hour aggregates

# Downsampled tier resolution (default: 1m, minimum: 30s)
downsample_resolution = "1m"
# Archived tier resolution (default: 1h, minimum: 15m)
archive_resolution = "1h"
```

### 6.2 Scraping Configuration

```toml
# Per-app overrides (in the main cluster.toml / app definition)
[app.my-api]
image = "myapp:v2"
port = 8080
metrics = "/prom/metrics"       # Override path (default: "/metrics")
metrics_interval = "10s"        # Override interval (default: "30s")

# Disable scraping for a specific app:
[app.static-assets]
port = 80
metrics = false

# Global scrape settings (in mayo config)
[mayo.scrape]
default_interval = "30s"        # Default scrape interval for all apps
timeout = "5s"                  # Per-scrape timeout
max_samples_per_scrape = 10000  # Safety limit per scrape target
```

### 6.3 Alert Configuration

```toml
# Notification destinations
[alerts.notify]
webhook = "https://hooks.slack.com/services/T.../B.../xxx"

# Or multiple destinations:
[[alerts.notify.destinations]]
type = "webhook"
url = "https://hooks.slack.com/services/T.../B.../xxx"
severity = ["critical", "warning"]

[[alerts.notify.destinations]]
type = "webhook"
url = "https://events.pagerduty.com/v2/enqueue"
severity = ["critical"]

# Alert evaluation interval (default: 15s)
[alerts]
eval_interval = "15s"

# Custom alert rules:
[[alert]]
name = "api.error_rate_high"
expr = "rate(http_requests_total{status=~'5..'}[5m]) / rate(http_requests_total[5m]) > 0.05"
for = "3m"
severity = "critical"
message = "API error rate above 5% for 3 minutes"

[[alert]]
name = "queue.depth"
expr = "job_queue_depth > 10000"
for = "10m"
severity = "warning"
message = "Job queue depth exceeds 10,000"

# Per-app alert suppression/tuning (in the app definition):
[app.batch-worker.alerts]
cpu.idle = false
memory.low = { threshold = 95 }
```

### 6.4 Aggregation Configuration

```toml
[mayo.aggregation]
# How often nodes push rollups to council aggregators (default: 60s)
push_interval = "60s"

# Rollup retention on council aggregators
rollup_retention_1m = "24h"     # 1-minute rollups
rollup_retention_1h = "30d"     # 1-hour rollups

# Max rollup payload size (safety limit)
max_rollup_size_bytes = 1048576  # 1MB
```

---

## 7. Failure Modes

### 7.1 Storage Exhaustion

**Trigger:** The Mayo data directory reaches the `max_storage` limit (default: 5% of disk, or explicit `max_storage` value).

**Behaviour:**

1. Mayo stops ingesting new samples and logs a warning.
2. Emergency retention pruning runs immediately: the oldest full-resolution block is deleted, then the oldest downsampled block, working backward until 10% headroom is recovered.
3. If pruning cannot free sufficient space (e.g., only archived data remains and it is within minimum retention), Mayo enters degraded mode: it continues collecting samples in a small in-memory ring buffer (last 5 minutes) but does not persist to disk.
4. The `disk.filling` built-in alert fires (independent of Mayo storage -- it monitors the entire filesystem).
5. A dedicated `mayo_storage_exhausted` alert fires with severity `critical`.

**Recovery:** Once disk space is freed (by operator intervention, external log cleanup, or the passage of time allowing retention pruning to reclaim more blocks), Mayo resumes normal disk writes. The in-memory ring buffer is flushed to disk. There will be a gap in persisted data for the degraded period, visible in queries as missing data points.

### 7.2 Aggregator Council Member Failure

**Trigger:** A council member acting as a rollup aggregator crashes or becomes unreachable.

**Behaviour:**

1. Nodes assigned to the failed aggregator detect the failure via Mustard gossip (membership change) or via gRPC push failure (connection refused / timeout).
2. Nodes recompute their aggregator assignment based on the updated council membership. They begin pushing rollups to their new aggregator.
3. The new aggregator has no historical rollups for the reassigned nodes. A coverage gap exists for the reassignment period (typically <60 seconds if gossip propagation is fast).
4. Nodes include their previous 5 minutes of rollups in the first push to the new aggregator to minimise the gap.
5. Cluster-wide queries during the gap period return partial data with a warning annotation.

**Recovery:** When the failed council member recovers and rejoins the council, some nodes are reassigned back. The same 5-minute backfill applies.

### 7.3 Stale Rollups

**Trigger:** A node is alive but its rollup push to the council aggregator fails silently (e.g., network partition to the aggregator but not to the rest of the cluster), or the node's clock is significantly skewed.

**Behaviour:**

1. The council aggregator tracks the last-seen rollup timestamp per node.
2. If no rollup is received from a node for 3 push intervals (3 minutes default), the aggregator marks that node's data as `stale`.
3. Stale node data is excluded from cluster-wide query results (rather than returning outdated numbers), and a warning annotation is included in the response.
4. The aggregator emits a `mayo_aggregator_stale_node` metric with the node ID as a label.

**Recovery:** When rollups resume (network heals, node restarts), the stale marker is cleared and the node's data is included in subsequent queries.

### 7.4 Scrape Target Timeout

**Trigger:** An application's `/metrics` endpoint becomes slow or unresponsive.

**Behaviour:**

1. If a scrape exceeds the timeout (default: 5 seconds), the scrape is aborted and no samples are ingested for that interval.
2. After 3 consecutive timeouts, a warning is logged: `"app.web instance web-3: metrics scrape timeout (3 consecutive, disabling for 5m)"`.
3. Scraping for that instance is backed off for 5 minutes, then retried.
4. A `mayo_scrape_timeout_total{app, instance}` counter is incremented.

**Recovery:** If the next probe after backoff succeeds, normal scraping resumes at the configured interval.

### 7.5 Node Failure (Data Loss)

**Trigger:** A node crashes permanently, losing its local Mayo data.

**Behaviour:**

- All historical full-resolution data for that node is lost. This is an accepted trade-off of the per-node storage architecture.
- Council aggregators retain rollup data for the failed node (1-minute rollups for 24 hours, 1-hour rollups for 30 days), so cluster-wide dashboards continue to reflect the node's contribution.
- If apps from the failed node are rescheduled to other nodes, they begin generating new metrics on those nodes. There is a gap in per-app history spanning the failure + reschedule period.

**Mitigation for teams requiring full durability:** Use the Prometheus remote-read API to federate data into an external Thanos/Cortex cluster with replicated storage.

---

## 8. Security Considerations

### 8.1 Metrics Access Control

- **Internal API (Brioche, Relish, inter-node queries):** Secured via mutual TLS using the Sesame PKI. All inter-node gRPC calls (query fan-out, rollup push) use mTLS with certificates issued by the cluster CA. Only nodes with valid cluster certificates can query or push metrics.
- **Prometheus remote-read API:** Protected by the same mTLS requirement by default. For external Prometheus instances, operators must provide the cluster CA certificate and a client certificate (issued via `relish cert issue --for prometheus`). Alternatively, operators can enable token-based authentication for the remote-read endpoint.
- **Per-namespace isolation (future):** The query API enforces namespace-scoped access when the caller is authenticated as a namespace-scoped identity (e.g., a workload identity token from app.web in namespace `team-a` can only query metrics for apps in `team-a`). Cluster-wide queries require a cluster-scoped identity.
- **Metric label scrubbing:** Application-scraped metrics are stored as-is. Mayo does not inject or strip labels from application metrics. Operators concerned about label cardinality explosion (a common Prometheus anti-pattern) can configure `max_samples_per_scrape` to limit ingest.

### 8.2 Alert Webhook Authentication

- **HMAC signing:** When an `AlertNotificationDestination` has a `secret` configured, the webhook payload is signed with HMAC-SHA256. The signature is included in the `X-Mayo-Signature-256` HTTP header. The receiving service can verify authenticity by computing HMAC-SHA256 over the raw request body with the shared secret.
- **TLS enforcement:** Webhook URLs must use HTTPS. HTTP URLs are rejected at configuration validation time with a clear error message. An `--allow-insecure-webhooks` flag exists for development/testing only.
- **Secret storage:** Webhook secrets are stored encrypted in the Raft state (using the same encryption as Reliaburger secrets, Section 11). They are decrypted in memory only on the node evaluating alerts.

---

## 9. Performance

### 9.1 Storage Footprint

At 10-second collection intervals with 100 apps per node, a busy node generates approximately:

| Component | Estimate |
|-----------|----------|
| Auto-collected metrics (100 apps x ~10 metrics x 8640 samples/day) | ~15MB/day (Gorilla-encoded) |
| Auto-collected node metrics (~15 metrics x 8640 samples/day) | ~0.2MB/day |
| Scraped application metrics (varies; estimate 500 series x 2880 samples/day at 30s) | ~10MB/day |
| Ingress metrics (estimate 50 routes x 5 metrics x 8640 samples/day) | ~3MB/day |
| Downsampled tier (all series, 1/6th sample count, +rollup fields) | ~8MB/day |
| Archived tier (all series, 1/360th sample count, +rollup fields) | ~0.5MB/day |
| **Total** | **~37-100MB/day** |

The whitepaper specifies **50-100MB/day per busy node** as the expected range, accounting for variance in application metric cardinality.

### 9.2 Query Latency Targets

| Query Type | Target | Mechanism |
|------------|--------|-----------|
| Single-app query (CPU for app.web, 3-10 nodes) | < 50ms | Fan out only to nodes running the app; parallel query; local index lookup |
| Cluster-wide query (top 10 CPU consumers, 10K nodes) | < 500ms | Fan out to 5-7 council aggregators; query pre-aggregated rollups |
| Local node query (single node's data, via CLI) | < 10ms | Direct local TSDB read, no network |
| Prometheus remote-read (single series, 1h range) | < 100ms | Direct TSDB read + protobuf serialisation |

### 9.3 Scrape Overhead

| Metric | Target |
|--------|--------|
| CPU overhead per scrape (per instance) | < 0.5ms of CPU time |
| Memory overhead per scrape target | ~4KB (connection buffer) + parsed samples |
| Network overhead per scrape (100 metrics) | ~5KB (Prometheus exposition format is compact) |
| Concurrent scrape goroutines per node | Capped at 50 (to avoid overwhelming the node) |

### 9.4 Aggregation Overhead

| Metric | Target |
|--------|--------|
| Rollup generation (per node, per push) | < 5ms CPU |
| Rollup payload size (100 apps) | ~10-50KB (serialised protobuf) |
| Council aggregator memory (rollups from 2000 nodes, 24h 1-min retention) | ~200MB |
| Cluster-wide query merge (5 aggregator results) | < 50ms |

---

## 10. Testing Strategy

### 10.1 Retention Tier Transitions

**Unit tests:**

- Verify that full-resolution samples are correctly aggregated into `AggregationRollup` values (min, max, sum, count, counter_increase).
- Verify that counter resets within an aggregation window are handled correctly (`counter_increase` should reflect the total increase including resets).
- Verify that the downsampler processes exactly the closed time window and does not include samples from the next window.
- Verify that the archiver correctly aggregates 60 one-minute rollups into one one-hour rollup.

**Integration tests:**

- Ingest 2 hours of synthetic metrics at 10-second intervals. Verify that after 1 minute, the downsampled tier contains the expected rollups. Verify that after 1 hour, the archived tier contains the expected rollups.
- Advance the clock past the full-tier retention (24h). Verify that full-resolution blocks are pruned. Verify that downsampled data is still available.
- Advance the clock past the downsampled retention (7d). Verify that downsampled blocks are pruned. Verify that archived data is still available.
- Verify that `max_storage` enforcement correctly triggers emergency pruning.

### 10.2 Aggregation Accuracy

**Unit tests:**

- Generate `NodeRollup` payloads with known values. Push to a mock aggregator. Query and verify that `sum()`, `avg()`, `max()`, `min()` produce mathematically correct results.
- Verify that aggregation is commutative and associative: the result of merging partial aggregations from 5 aggregators should equal the result of aggregating all data at once.

**Integration tests:**

- Deploy a test cluster with 5 nodes. Generate deterministic metrics on each node (known CPU values). Query `sum(app_cpu_usage_seconds_total)` via the cluster-wide path and verify it equals the sum of individual node values.
- Kill a council aggregator mid-query. Verify that the query returns partial results with a warning annotation rather than failing entirely.
- Reassign nodes to a new aggregator and verify the 5-minute backfill produces continuous data.

### 10.3 Alert Firing Verification

**Unit tests:**

- Verify that an alert with `for = "5m"` transitions from `Inactive` -> `Pending` -> `Firing` after exactly 5 minutes of continuous threshold breach.
- Verify that a momentary dip below threshold during the `for` period resets the alert to `Inactive`.
- Verify that per-app suppression (`cpu.idle = false`) prevents the alert from firing for the suppressed app but not for other apps.
- Verify that per-app threshold tuning (`memory.low = { threshold = 95 }`) uses the overridden value.

**Integration tests:**

- Deploy a test app that consumes 90% of its memory limit. Verify that the `memory.low` alert fires within the expected timeframe (default: 5 minutes).
- Deploy a test app that triggers an OOM kill. Verify that the `oom.kill` alert fires immediately (no `for` duration on OOM).
- Configure a webhook notification destination. Trigger an alert. Verify that the webhook receives the expected JSON payload with correct labels, severity, and timestamps.
- Configure a per-app suppression. Verify that the suppressed alert does not fire for the suppressed app, does not produce a webhook notification, and is not visible in `relish alerts`.

### 10.4 Prometheus Compatibility Tests

- Scrape a test app that exposes all 4 Prometheus metric types (counter, gauge, histogram, summary). Verify correct parsing and storage.
- Query Mayo via the Prometheus remote-read API using an external Prometheus instance. Verify that data round-trips correctly.
- Evaluate each supported PromQL function and operator against known data sets and compare results with a reference Prometheus implementation.

### 10.5 Scraping Tests

- Deploy an app with a `/metrics` endpoint. Verify that scraping begins automatically within one collection interval.
- Deploy an app without a `/metrics` endpoint. Verify that no scrape errors are logged and no scrape metrics are created.
- Deploy an app with a non-standard metrics path (`metrics = "/prom/metrics"`). Verify that the override is respected.
- Set `metrics = false` for an app. Verify that no probing occurs.
- Deploy an app whose `/metrics` endpoint takes 10 seconds to respond. Verify timeout handling and backoff behaviour.

---

## 11. Prior Art

### 11.1 Prometheus

[Prometheus](https://prometheus.io/) is the de facto standard for cloud-native metrics. Its TSDB design (described in [Fabian Reinartz's Prometheus TSDB design doc](https://fabxc.org/tsdb/)) is the primary inspiration for Mayo's on-disk format: block-based storage, Gorilla-style chunk encoding, and posting-list label indexes.

**What we borrow:** Prometheus data model (metric name + labels + samples), exposition format (for scraping), PromQL expression language (subset), remote-read API protocol.

**What we do differently:** Prometheus requires a central server that scrapes all targets and stores all data. At scale, this becomes a bottleneck requiring complex federation, sharding (Cortex, Thanos), or hierarchical federation. Mayo embeds the TSDB in every node, eliminating the central server entirely. There is no "Prometheus server" to operate, scale, or secure.

### 11.2 Thanos

[Thanos](https://thanos.io/tip/thanos/design.md/) extends Prometheus with a sidecar model that uploads blocks to object storage, enabling long-term retention and global querying across multiple Prometheus instances. Thanos Query fans out to multiple Thanos Store instances, similar to Mayo's query fan-out.

**What we borrow:** The concept of hierarchical query fan-out and deduplication of overlapping data from multiple sources.

**What we do differently:** Thanos requires deploying and operating multiple components (Sidecar, Store, Query, Compactor, Ruler) alongside Prometheus. Mayo provides equivalent functionality (per-node storage, hierarchical querying, downsampling/compaction, alert evaluation) in a single embedded component with no external dependencies or object storage.

### 11.3 VictoriaMetrics

[VictoriaMetrics](https://docs.victoriametrics.com/) is a high-performance TSDB that can serve as a long-term remote-write destination for Prometheus. Its architecture favours a centralized (or clustered) storage model with excellent compression and query performance.

**What we borrow:** Aggressive compression techniques and the emphasis on low storage overhead per sample.

**What we do differently:** VictoriaMetrics is a standalone database that must be deployed and operated separately. Mayo is embedded and requires no separate deployment. VictoriaMetrics uses a centralized storage model; Mayo uses fully distributed per-node storage.

### 11.4 Cortex

[Cortex](https://cortexmetrics.io/) provides horizontally scalable, multi-tenant Prometheus-compatible storage. It uses a complex microservices architecture (Distributor, Ingester, Querier, Store Gateway, Compactor) backed by object storage and a key-value store.

**What we borrow:** The concept of query fan-out to multiple storage backends, and the awareness that central Prometheus does not scale.

**What we do differently:** Cortex solves the scaling problem by adding complexity (many microservices, external dependencies). Mayo solves it by distributing storage to the edge (every node) and using hierarchical aggregation for cluster-wide queries, requiring zero additional infrastructure.

### 11.5 InfluxDB

[InfluxDB](https://www.influxdata.com/) is a general-purpose TSDB with a custom query language (InfluxQL/Flux). It uses a Time-Structured Merge Tree (TSM) storage engine.

**What we borrow:** The concept of configurable retention policies with automatic downsampling, and the tiered storage model.

**What we do differently:** InfluxDB is a standalone database with its own query language. Mayo uses the Prometheus data model and PromQL (which operators already know), and is embedded rather than standalone.

### 11.6 Summary of Design Decisions

| Decision | Prior art approach | Mayo approach | Rationale |
|----------|--------------------|---------------|-----------|
| Data model | Prometheus labels+samples (universal) | Same | Industry standard; PromQL ecosystem compatibility |
| Query language | PromQL (Prometheus), InfluxQL (InfluxDB) | PromQL subset | Existing operator knowledge transfer; ecosystem compatibility |
| Storage topology | Central server (Prometheus), sharded cluster (Cortex/VM) | Per-node, no central DB | Eliminates operational complexity; scales linearly; no SPOF |
| Cluster-wide queries | Scatter-gather all instances (Thanos), query sharded cluster (Cortex) | Hierarchical aggregation via council | Bounds fan-out at O(council_size) not O(cluster_size) |
| Deployment | Separate infrastructure (all) | Embedded in Bun binary | Zero operational overhead; batteries-included philosophy |
| Long-term retention | Object storage (Thanos, Cortex) | Local tiered downsampling + optional federation | No external storage dependency; federation available for teams that want it |

---

## 12. Libraries & Dependencies

### 12.1 Rust Crates

| Crate | Purpose | Notes |
|-------|---------|-------|
| **Custom TSDB engine** | Core storage, indexing, compaction | Written in Rust specifically for Mayo. The Prometheus TSDB Go code is the design reference, but a direct port is not possible (Go -> Rust) and the embedded use case allows significant simplification (single-writer, no WAL replication, no remote-write ingest). |
| `sled` or `rocksdb` (via `rust-rocksdb`) | Label index storage | Persistent sorted key-value store for the label posting lists and label-to-MetricId mappings. `sled` is pure Rust (simpler build, no C++ dependency) but less battle-tested than RocksDB. Decision deferred to implementation (see Open Questions). |
| `zstd` (`zstd` crate) | Block compression | Zstd compression for downsampled and archived tier blocks. Level 3 provides a good balance of compression ratio and speed. |
| `promql-parser` | PromQL parsing | Parses PromQL expressions into an AST for alert evaluation and query processing. If no mature Rust crate exists, a custom parser will be implemented using `nom` or `pest`. |
| `hyper` + `tonic` | HTTP + gRPC server | `hyper` serves the Prometheus remote-read API (HTTP/1.1 protobuf) and the `/metrics` scrape endpoint. `tonic` handles inter-node gRPC for query fan-out and rollup push. |
| `prost` | Protobuf serialisation | Encodes/decodes Prometheus remote-read protobuf messages and internal rollup messages. |
| `memmap2` | Memory-mapped file access | Memory-maps block index files and chunk files for zero-copy reads during queries. |
| `tokio` | Async runtime | Drives the scrape scheduler, query fan-out, rollup push, and compaction background tasks. Already used by Bun. |
| `reqwest` or `hyper` (client) | HTTP client | Scrapes application `/metrics` endpoints and delivers webhook notifications. |
| `nom` or `pest` | Parser combinators | For parsing Prometheus exposition format from scrape targets (text/plain Content-Type with `# HELP`, `# TYPE` lines, metric lines). |
| `ulid` | ULID generation | Generates unique, lexicographically sortable block IDs (used as block directory names). |
| `crc32fast` | Checksum | WAL segment and block integrity verification. |

### 12.2 Build Considerations

All dependencies are compiled into the single Bun binary. There are no runtime dependencies on external libraries. The choice between `sled` (pure Rust) and `rocksdb` (C++ via FFI) affects the build:

- **`sled`:** Pure Rust, simpler cross-compilation, smaller binary delta (~2MB). Less mature for heavy write workloads.
- **`rocksdb`:** Battle-tested, excellent write throughput, but requires C++ toolchain for cross-compilation and adds ~5MB to the binary.

---

## 13. Open Questions

### 13.1 PromQL Completeness Level

**Question:** How complete should the PromQL implementation be?

**Current plan:** Implement the subset described in Section 5.6 (instant/range selectors, common aggregations, `rate`/`irate`/`increase`/`delta`/`histogram_quantile`, arithmetic/comparison/logical operators). This covers the vast majority of real-world alert rules and dashboard queries.

**Deferred:** Subquery syntax (`metric[1h:5m]`), `label_replace()`, `label_join()`, `predict_linear()`, `holt_winters()`, `absent()`, `absent_over_time()`, `scalar()`, `vector()`, recording rules. These can be added incrementally based on user demand.

**Risk:** Teams migrating from Prometheus may have complex alert expressions that use unsupported functions. Mitigation: Mayo logs a clear error at config load time identifying the unsupported function, and the Prometheus remote-read API allows teams to continue using full PromQL in their external Prometheus.

### 13.2 Federation API Design

**Question:** Should Mayo expose a Prometheus-compatible federation endpoint (`/federate`) in addition to the remote-read API?

**Arguments for:** The `/federate` endpoint is simpler to configure in Prometheus (just another scrape target) and is widely understood. Many teams use federation today.

**Arguments against:** The remote-read API is more capable (supports time-range queries, not just instant snapshots) and is the modern Prometheus recommendation for cross-cluster data access. Supporting both increases API surface.

**Current leaning:** Implement remote-read first (higher value, more capable). Add `/federate` if user demand warrants it. The implementation cost is low (it is essentially a query + exposition format serialisation).

### 13.3 Exemplar Support

**Question:** Should Mayo support Prometheus exemplars (trace-ID-annotated samples)?

**Arguments for:** Exemplars bridge the metrics-to-traces gap. If Reliaburger ever adds distributed tracing, exemplars would allow clicking from a latency spike on a dashboard to the specific trace.

**Arguments against:** Reliaburger does not currently include a tracing component. Exemplar support adds storage and API complexity for a feature that has no consumer today.

**Current leaning:** Defer. Design the storage format to be extensible (reserve a field in the chunk encoding for per-sample metadata), but do not implement exemplar ingestion or query until a tracing component is designed.

### 13.4 Index Storage Backend

**Question:** Use `sled` (pure Rust) or `rocksdb` (C++ FFI) for the label index?

**Arguments for sled:** Pure Rust, simpler build, no C++ dependency, aligns with the "single binary, minimal dependencies" philosophy.

**Arguments for rocksdb:** Battle-tested at scale, superior write amplification characteristics, well-understood performance profile. Used by Prometheus (via LevelDB family), VictoriaMetrics, and many production TSDBs.

**Current leaning:** Start with `sled` for development velocity and build simplicity. Benchmark under realistic workloads (100 apps, 30s scrape interval, 10s collection interval, 24h of data). Switch to `rocksdb` if `sled` does not meet performance targets. The label index interface will be abstracted behind a trait to allow backend substitution.

### 13.5 Histogram and Summary Aggregation

**Question:** How should histogram and summary metrics be aggregated in the downsampled and archived tiers?

**Problem:** Prometheus histograms are composed of multiple counter series (`_bucket`, `_sum`, `_count`). Downsampling each bucket independently is correct for `histogram_quantile()` computation. Prometheus summaries (pre-computed quantiles) cannot be meaningfully aggregated across time windows or instances -- this is a known limitation of the Prometheus data model.

**Current plan:** Downsample histogram `_bucket`, `_sum`, `_count` series independently (preserving the ability to compute quantiles over longer time ranges). For summaries, store only the latest value in each aggregation window (since quantiles cannot be aggregated). Document this limitation.

### 13.6 Cross-Cluster Query

**Question:** Should Mayo support querying across multiple Reliaburger clusters?

**Current plan:** Out of scope for v1. Teams that need cross-cluster metrics should federate each cluster's data into a shared Thanos/Cortex instance via the remote-read API. A future "multi-cluster Brioche" could aggregate across clusters at the UI layer.
