# Mayo: Embedded Metrics Store

**Component:** Mayo (Metrics store + Built-In Alerts)
**Binary:** Embedded in Bun (the single-node agent)
**Status:** Design (this document reconciled against the shipped code)
**Whitepaper Reference:** Section 15

---

## 1. Overview

Mayo is Reliaburger's embedded, per-node metrics store. It provides a metrics pipeline -- collection, storage, querying, and alerting -- with zero configuration required. Every Bun instance runs a Mayo store that:

- **Auto-collects** node infrastructure metrics (CPU, memory, disk, network) and per-process metrics (CPU, memory) for every running instance.
- **Stores data locally** as flat Parquet files (`metrics_NNNNNN.parquet`), queried with **DataFusion SQL**, and prunes them by a single `retention_days` window.
- **Pushes 1-minute rollups** to its council parent over the reporting transport, so a council member can serve a partial cluster view.
- **Evaluates built-in threshold alert rules** (metric, operator, threshold, duration), with webhook, Slack, and PagerDuty notification.

The result is a batteries-included metrics store embedded in the single Bun binary, with no external database and no configuration for the common case.

A note on this document: the storage engine is **Parquet + DataFusion SQL**, not a custom Gorilla/ULID TSDB, and the query language is **SQL**, not PromQL. Several capabilities are now wired: outbound Prometheus scraping of `[[metrics.scrape_targets]]`, cross-council query fan-out and merge, ingress request/response counters, and rollup retention pruning. Others the original design imagined -- remote-read federation, tiered downsampling, PromQL alert expressions, per-app alert tuning -- remain **not wired into a running system** and are each flagged below with **Status: planned -- not yet implemented.**

---

## 2. Dependencies

### Internal Components

| Component | Relationship |
|-----------|-------------|
| **Bun** (node agent) | Host process. Bun's collection loop samples node and per-process metrics (via the `sysinfo` crate) into the local Mayo store, drives the periodic flush to Parquet, runs the alert evaluation loop, and drives retention/disk-pressure pruning. |
| **Council** (Raft consensus group) | Each worker node pushes a 1-minute `NodeRollup` to its assigned council parent over the reporting transport. The council member stores rollups in a separate rollup store, served at `/v1/metrics/rollup` and `/v1/metrics/cluster`. **Cross-council query fan-out and merge is implemented**: `/v1/metrics/cluster` and `/v1/metrics/app` fan out across council voters and sum the results (see §3.3). |
| **Brioche** (web UI) | Queries Mayo over the internal HTTP API (`/v1/metrics*`) for dashboard rendering. |
| **Relish** (CLI) | `relish` queries the per-app metrics endpoint over HTTP. |
| **Mustard** (gossip) | Provides cluster membership and node identity, used to determine which council parent a node reports rollups to. |
| **Meat** (scheduler) | Provides the app-to-node placement map. The `/v1/metrics/app` endpoint uses it to fan single-app queries out to the nodes running the app and sum the results. |
| **Wrapper** (ingress proxy) | **Implemented.** The proxy maintains a process-global `IngressMetrics` counter (total, per-status-class, in-flight); Bun's collection loop folds `global_ingress_metrics().snapshot()` into the time series as `ingress_requests_total`, `ingress_responses_Nxx`, and `ingress_requests_in_flight`. Per-route duration histograms remain future work. |

### External Interfaces

| Interface | Direction | Status |
|-----------|-----------|--------|
| Webhook notifications | Outbound | **Shipped.** Alert notifications dispatched to generic HTTP, Slack, or PagerDuty destinations. |
| Prometheus remote-read API | Inbound | **Status: planned -- not yet implemented.** There is no `remote_read` endpoint. |
| Prometheus scrape endpoint (`/metrics` exposition) | Inbound | **Status: planned -- not yet implemented.** Mayo does not expose its data in Prometheus exposition format. |
| Application `/metrics` scraping | Outbound (node-local) | **Implemented.** When `[[metrics.scrape_targets]]` is non-empty, Bun spawns a loop that scrapes each target's `/metrics` every `scrape_interval_secs`, parses the Prometheus text (`src/mayo/scrape.rs`), and ingests the samples tagged with the target's `job`. An empty target list means no loop is spawned. |

---

## 3. Architecture

### 3.1 Per-Node Storage (No Central Database)

Each node stores its own metrics locally. There is no central metrics database. This design means:

- Metrics storage scales linearly with cluster size.
- There is no single bottleneck or point of failure for metrics.
- A node failure loses only that node's local historical data (the 1-minute rollups its council parent holds provide a coarse cluster-level view).
- No inter-node replication of raw metric data is required.

```
┌──────────────────────────────────────────────────────────────────┐
│  Node (Bun)                                                      │
│                                                                  │
│  ┌──────────────────────────┐                                    │
│  │ Collection loop (sysinfo)│  node CPU/mem/disk/net,            │
│  │  every collection_interval│  per-process CPU/mem               │
│  └────────────┬─────────────┘                                    │
│               ▼                                                  │
│       ┌────────────────────────────────┐                         │
│       │        Mayo store              │                         │
│       │  in-memory buffer ─flush─▶      │                         │
│       │  metrics_NNNNNN.parquet files   │                         │
│       │  queried via DataFusion SQL     │                         │
│       │  (buffer ∪ on-disk Parquet)     │                         │
│       └───────────────┬────────────────┘                         │
│                       │                                          │
│           ┌───────────┼─────────────┐                            │
│           ▼           ▼             ▼                            │
│   Local SQL query  Rollup push   Retention prune                 │
│   (/v1/metrics*)   to council    (retention_days;               │
│                    parent        disk-pressure)                  │
└──────────────────────────────────────────────────────────────────┘
```

Inserts land in an in-memory buffer. On flush the buffer becomes an Arrow `RecordBatch`, is written to a new `metrics_NNNNNN.parquet` file, and dropped from memory. Queries register the on-disk Parquet directory plus the unflushed buffer as a DataFusion `metrics` table (columns `timestamp UInt64`, `metric_name Utf8`, `labels Utf8` (a JSON object), `value Float64`) and run SQL over the union. A corrupt or truncated Parquet file is skipped with a log rather than failing the whole query.

### 3.2 Retention

**Status of the original three-tier design (10s full / 1-min downsampled / 1-hour archived, 24h/7d/90d): planned -- not yet implemented.** There is no downsampler, no archiver, and no tiered storage. What ships is a single flat retention window.

Retention is a single `retention_days` value (default **7**). Two paths prune Parquet files:

- `MayoStore::prune(before)` deletes any file whose **newest datapoint** is older than the cutoff. The newest timestamp is read from the file's Parquet row-group statistics, not the file's mtime, so a touched/copied file with old data is still pruned and a file with recent data is kept regardless of mtime.
- The disk-pressure task (`src/bun/disk_pressure.rs`) prunes oldest-first by file **mtime** when either `retention_days` has elapsed or `max_storage_mb` is exceeded (see §7.1). When an export destination is configured it exports a file before pruning it.

### 3.3 Rollup Push to Council Members

**What ships:** each worker node runs a rollup worker (`src/mayo/rollup_worker.rs`) that, every `rollup_interval_secs` (default 60), generates a `NodeRollup` -- 1-minute (min, max, sum, count) aggregates of the local metrics -- and sends it to its assigned council parent over the reporting transport (bincode-framed messages, `ReportingMessage::MetricsRollup`, not gRPC). The council member stores received rollups in a separate rollup store, exposed at `/v1/metrics/rollup` (this member's own data) and `/v1/metrics/cluster`.

**What is planned -- not yet implemented:**

- **Cross-council query fan-out and merge.** `src/mayo/query_fanout.rs` contains `fan_out_cluster_query`/`fan_out_app_query` and the merge functions, but nothing calls them. `/v1/metrics/cluster` returns only the handling council member's *local* rollup data; it does not fan out to the other council members or merge their partial sums. So a "cluster-wide" query today reflects one council member's subset, not the whole cluster.
- **Single-app query fan-out via the Meat placement map.** `/v1/metrics/app/{app}/{namespace}` filters the *local* store by the `namespace/app` label. It does not consult placement or query the other nodes running the app.
- **`rollup_retention_hours`.** Defaults to 24. Bun's disk-pressure loop calls `RollupStore::prune_expired` each tick to drop rollups older than this window (0 keeps all).

### 3.4 Prometheus-Compatible Remote-Read API

**Status: planned -- not yet implemented.** There is no `/v1/read` remote-read endpoint and no `/metrics` Prometheus-exposition endpoint. Mayo's data is reachable only through the `/v1/metrics*` JSON endpoints (see §5.7). External-Prometheus federation is future work.

---

## 4. Data Structures

### 4.1 Core Metric Types

These are the types the shipped collector, store, and alert engine use (`src/mayo/types.rs`).

```rust
use std::collections::BTreeMap;

/// A metric name (e.g. `node_cpu_usage_percent`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MetricName(pub String);

/// A metric key: name + a set of labels.
///
/// Labels live in a `BTreeMap` so the same metric with the same labels
/// always produces the same key regardless of insertion order. On storage
/// the labels are serialised to a JSON string (the `labels` Parquet column).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MetricKey {
    pub name: MetricName,
    pub labels: BTreeMap<String, String>,
}

/// A single metric data point.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Seconds since Unix epoch (not milliseconds).
    pub timestamp: u64,
    pub value: f64,
}

/// The kind of metric. Note: only Gauge and Counter exist -- there are no
/// Histogram/Summary/Untyped kinds, because Mayo does not parse Prometheus
/// exposition into a running store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricKind {
    Gauge,
    Counter,
}
```

There is deliberately no `MetricDescriptor`, `TimeSeries`, `MetricId`, or interned label index: DataFusion plans over the Parquet columns directly, so Mayo carries no posting-list index of its own.

### 4.2 Rollup Types

The rollup worker and council rollup store use these (`src/mayo/rollup.rs`). One `NodeRollup` is generated per node per push interval and pushed to the council parent.

```rust
/// Min/max/sum/count aggregate of one metric series over the push window.
pub struct RollupAggregate {
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    pub count: u64,
}

/// One (metric, labels, aggregate) entry within a NodeRollup.
pub struct RollupEntry { /* metric name, labels, RollupAggregate */ }

/// A node's 1-minute rollup, pushed to its council parent.
pub struct NodeRollup { /* node id, timestamp (secs), Vec<RollupEntry> */ }
```

**Status of tiered aggregates: planned -- not yet implemented.** There is no `RetentionTier`, `TierName`, `Compression` (Gorilla/zstd) enum, `AggregationRollup.counter_increase`, downsampler, or archiver. Rollups are a single 1-minute summary pushed to the council, not an on-disk downsampling tier.

### 4.3 Alert Data Structures

Alert rules are **threshold rules**, not PromQL expressions (`src/mayo/alert.rs`).

```rust
/// A threshold alert rule.
pub struct AlertRule {
    pub name: String,
    /// The metric name to check (matched against the latest value per name).
    pub metric_name: String,
    pub threshold: f64,
    pub operator: AlertOperator,   // GreaterThan | LessThan
    /// How long the breach must hold before firing.
    pub for_duration: Duration,
    pub severity: AlertSeverity,   // Warning | Critical  (no Info)
    pub description: String,
}

pub enum AlertOperator { GreaterThan, LessThan }
pub enum AlertSeverity { Warning, Critical }

/// State machine per rule: Inactive → Pending → Firing.
pub enum AlertState {
    Inactive,
    Pending { since: SystemTime },
    Firing { since: SystemTime },
}
```

A rule fires when the latest value for `metric_name` breaches `threshold` under `operator` continuously for `for_duration`. Evaluation is three-way, not two-way: a *missing* metric is not treated as "recovered" -- a firing alert whose telemetry goes stale stays firing (only a genuine in-range reading resolves it), and a pending alert whose data vanishes drops back to inactive.

**Status: planned -- not yet implemented:** expression (PromQL) rules with `expr`; the `Info` severity; per-app suppression/tuning (`AlertOverride`, `is_builtin`, `app_overrides`); and the `!=`/`>=`/`<=`/`==` operators (only strict `>` and `<` exist).

Notification destinations are plain config structs (`AlertDestination` in `src/config/node.rs`): `dest_type` (`"webhook"` | `"slack"` | `"pagerduty"`), `url`, an optional `severity` filter list, and an optional HMAC `secret`.

### 4.4 On-Disk Format

**Status of the block/WAL/Gorilla/ULID design below: planned -- not yet implemented.** There is no WAL, no memory-mapped head chunk, no ULID blocks, no posting-list index, no Gorilla encoding, and no `downsampled/`/`archived/` directories.

What actually lands on disk is a flat directory of self-describing Parquet files:

```
<storage.metrics>/                 # default /var/lib/reliaburger/metrics
├── metrics_000000.parquet
├── metrics_000001.parquet
├── metrics_000002.parquet
└── ...
```

Each file is one flush of the in-memory buffer, written with Arrow's `ArrowWriter`. Every file carries the same schema:

| Column | Arrow type | Meaning |
|--------|-----------|---------|
| `timestamp` | `UInt64` | seconds since Unix epoch |
| `metric_name` | `Utf8` | e.g. `node_cpu_usage_percent` |
| `labels` | `Utf8` | the label `BTreeMap` serialised as a JSON object |
| `value` | `Float64` | the sample value |

File names come from a counter seeded one past the highest existing `metrics_NNNNNN.parquet`, so a restart resumes numbering instead of clobbering a previous run. Parquet's own column compression is the only compression applied -- there is no zstd wrapper managed by Mayo. Retention deletes whole files (§3.2); there is no sample-level tombstoning.

---

## 5. Operations

### 5.1 Auto-Collection

Bun's collection loop samples metrics at `collection_interval_secs` (default 10) via the cross-platform `sysinfo` crate (`src/mayo/collector.rs`). It works on Linux and macOS without cgroup/`/proc`-specific code. Every sample carries whatever labels the collector attaches; there is no automatic `node`/`instance` label injection.

**Per-node metrics** (9 gauges, no labels):

| Metric | Source |
|--------|--------|
| `node_cpu_usage_percent` | `sysinfo` global CPU usage |
| `node_memory_used_bytes` | `sysinfo` used memory |
| `node_memory_total_bytes` | `sysinfo` total memory |
| `node_disk_used_bytes` | sum over all disks |
| `node_disk_total_bytes` | sum over all disks |
| `node_network_rx_bytes` | sum over all interfaces |
| `node_network_tx_bytes` | sum over all interfaces |
| `node_network_rx_packets` | sum over all interfaces |
| `node_network_tx_packets` | sum over all interfaces |

Note: `node_memory_usage_percent` and `node_disk_usage_percent` (which the built-in alerts reference) are **not stored** -- they are derived on the fly at alert-evaluation time from the `used`/`total` pairs within one label set (see §5.6).

**Per-process metrics** (2 per running instance that has a PID):

| Metric | Source | Labels |
|--------|--------|--------|
| `process_cpu_percent` | `sysinfo` process CPU | `app` = `"{namespace}/{app}"`, `pid` |
| `process_memory_bytes` | `sysinfo` process RSS | `app` = `"{namespace}/{app}"`, `pid` |

Instances without a PID are skipped. There are no counters here, no restart/OOM/throttle metrics, and no cgroup-derived limits.

**GPU metrics: none.** There is no `nvidia-smi` collection; no `*_gpu_*` metric is emitted.

**Ingress metrics: implemented (counters), planned (durations).** The Wrapper proxy maintains a process-global `IngressMetrics` and Bun's collection loop emits `ingress_requests_total`, `ingress_responses_1xx`..`5xx`, and `ingress_requests_in_flight`. Per-route breakdowns and `ingress_request_duration_seconds` histograms remain **planned -- not yet implemented**.

### 5.2 Prometheus Scraping

**Status: implemented (explicit targets); per-app auto-scrape planned.**

`src/mayo/scrape.rs` provides `parse_prometheus_text`, `scrape_endpoint` (an HTTP GET with a 5s timeout), and `scrape_once`, which Bun's scrape loop calls every `scrape_interval_secs` over the `[[metrics.scrape_targets]]` list -- each target's samples are ingested tagged with its `job`. A target that fails contributes nothing rather than failing the sweep, and an empty target list spawns no loop. Still **planned -- not yet implemented**: per-app `metrics`/`metrics_interval` config, auto-detection of `/metrics` endpoints, `max_samples_per_scrape`, and a scrape-timeout counter.

### 5.3 Downsampling

**Status: planned -- not yet implemented.** There is no downsampler and no archiver. Data is stored once as flat Parquet and pruned by age (§3.2). The two-stage full→downsampled→archived compaction described in earlier drafts does not exist.

### 5.4 Rollup Push to the Council

Every `rollup_interval_secs` (default 60) the rollup worker (`src/mayo/rollup_worker.rs`) reads the local Mayo store, generates a `NodeRollup` of 1-minute (min, max, sum, count) aggregates, and sends it to its council parent as a `ReportingMessage::MetricsRollup` over the reporting transport (bincode-framed, **not** gRPC or a bespoke TCP call). On the first push after a parent change it can include an extended (backfill) window.

The council member stores received rollups in a rollup store and serves them at `/v1/metrics/rollup` and `/v1/metrics/cluster`. Aggregator reassignment backfill and the "data unavailable" gap annotation described in earlier drafts are **planned -- not yet implemented**; today a reassignment simply starts a fresh rollup history on the new parent.

### 5.5 Query Fan-Out

**Status: implemented.** The fan-out helpers in `src/mayo/query_fanout.rs` (`fan_out_app_query`, `fan_out_cluster_query`, and the merge functions) are now wired into the handlers:

- `/v1/metrics/app/{app}/{namespace}` resolves the app's nodes from the Meat placement map and fans the query out to each, merging the results (falling back to the local store when there's no placement).
- `/v1/metrics/cluster` enumerates the Raft voters, maps each to its live membership address, and sums each member's `/v1/metrics/rollup` (falling back to local rollup data on a single node / no council).

So neither the single-app path nor the cluster-wide path fans out today. The query timeout, top-N merge, and unresponsive-node annotations belong to the planned design.

### 5.6 Alert Evaluation

The alert loop runs when `metrics.alerts_enabled` is set (default true), every `alerts.evaluation_interval_secs` (default **30**, not 15). Each tick, `gather_latest_values` reads the last 120 seconds of metrics, keeps the newest reading per `(metric_name, labels)` series, drops any reading older than a 90-second freshness bound, derives the memory/disk usage percentages *within* a label set, then collapses to one value per metric name (freshest series wins, deterministic tie-break). The evaluator (`src/mayo/alert.rs`) applies each rule to that value.

**Built-in alert rules (5, always active, node-scoped):**

| Name | Metric | Condition | Duration | Severity |
|------|--------|-----------|----------|----------|
| `cpu_throttle` | `node_cpu_usage_percent` | `> 90` | 5 min | Critical |
| `oom_risk` | `node_memory_usage_percent` | `> 85` | 2 min | Critical |
| `memory_high` | `node_memory_usage_percent` | `> 70` | 10 min | Warning |
| `disk_high` | `node_disk_usage_percent` | `> 80` | 5 min | Warning |
| `cpu_idle` | `node_cpu_usage_percent` | `< 5` | 30 min | Warning |

These are node-level rules against `node_*` metrics, not per-app rules. The state machine is `Inactive → Pending → Firing`, with the stale-telemetry handling described in §4.3.

**Custom / PromQL alert expressions: Status: planned -- not yet implemented.** There is no expression parser and no PromQL subset. Rules are the fixed threshold shape only; there is no `[[alert]]` TOML block, no `expr`, no range selectors, functions, or label matchers.

### 5.7 Alert Notification

When a rule transitions to `Firing` (or resolves), the webhook dispatcher (`src/mayo/webhook.rs`) builds a **provider-specific** body per destination and POSTs it. The endpoints Mayo itself exposes are the `/v1/metrics*` and `/v1/alerts` JSON endpoints; alert *delivery* is an outbound HTTP POST to the operator's destination.

**Generic (`dest_type = "webhook"`) payload:**

```json
{
  "version": "1",
  "alert": {
    "name": "cpu_throttle",
    "severity": "critical",
    "status": "firing",
    "message": "CPU usage above 90% for 5 minutes",
    "value": 95.3,
    "fired_at": 1700000000
  },
  "cluster": "prod",
  "timestamp": 1700000300
}
```

Note the real shape: `severity`/`status` are lowercase strings; `value` is the numeric metric value; `fired_at` and the top-level `timestamp` are **integer Unix seconds**, not RFC3339. There is **no** `labels` object and **no** `started_at` field. A resolve reuses the same schema with `status: "resolved"` and `fired_at: null`.

**Slack (`dest_type = "slack"`)** gets a `{ "attachments": [ ... ] }` body with a coloured bar (`danger` firing-critical, `warning` firing-warning, `good` resolved). **PagerDuty (`dest_type = "pagerduty"`)** gets an Events API v2 event (`routing_key` from `secret`, `event_action` `trigger`/`resolve`, a `dedup_key`, and a `payload` object on trigger). Posting one generic shape to all three would be silently dropped by Slack and PagerDuty, so each destination type is serialised to its own contract.

**Signing:** when a destination has a `secret`, the body is signed HMAC-SHA256 and the digest is sent as `X-Mayo-Signature-256: sha256=<hex>`. For PagerDuty the `secret` doubles as the routing key.

**Retry policy:** failed deliveries are retried 3 times with backoff (1s, 5s, 25s); after that the delivery is dropped with a log. There is no `mayo_alert_notification_failed_total` counter. Failure logs redact the URL to scheme+host so a Slack webhook path (which carries the secret) is never written to disk.

**HTTPS enforcement / `--allow-insecure-webhooks`: Status: planned -- not yet implemented.** HTTP webhook URLs are accepted; there is no scheme validation and no insecure-override flag.

### 5.8 Alert Suppression and Tuning Per-App

**Status: planned -- not yet implemented.** There is no per-app alert config (`[app.*.alerts]`), no suppression, and no threshold tuning. The five built-in rules are node-scoped and fixed; an operator cannot currently silence or retune them per app.

---

## 6. Configuration

All configuration is optional. The system operates with sensible defaults out of the box. The section is `[metrics]` (not `[mayo]`), with a separate `[alerts]` section; keys are integer `*_secs`/`*_days`/`*_mb`/`*_hours` values (there is no duration-string or byte-size parser here). Unknown keys are rejected (`deny_unknown_fields`).

### 6.1 Metrics Configuration (`[metrics]`)

```toml
[metrics]
# How often to collect node + process metrics (seconds). Default: 10.
collection_interval_secs = 10

# Days to retain metric Parquet files before pruning. Default: 7.
retention_days = 7

# How often to scrape Prometheus /metrics endpoints (seconds). Default: 30.
# NOTE: scraping is not wired (see §5.2); this key is currently inert.
scrape_interval_secs = 30

# Enable the built-in threshold alert loop. Default: true.
alerts_enabled = true

# Object-store URL for metric persistence. Empty = local filesystem.
# e.g. "s3://bucket/prefix". Default: "".
object_store_url = ""

# How often to push rollups to the council parent (seconds). Default: 60.
rollup_interval_secs = 60

# Intended rollup retention on council members (hours). Default: 24.
# NOTE: nothing reads this yet (see §3.3) — planned, currently inert.
rollup_retention_hours = 24

# Maximum local metrics Parquet storage (MB). 0 = unlimited (the default).
# When exceeded, exported files are pruned oldest-first (see §7.1).
max_storage_mb = 0

# Optional export destination for metrics Parquet files
# (local path, file://, s3://, gs://). Default: unset.
# export_path = "s3://bucket/metrics"
```

### 6.2 Scraping Configuration

**Status: partially implemented.** `scrape_interval_secs` now drives the scrape loop and `[[metrics.scrape_targets]]` declares the endpoints (see §5.2). Still absent: per-app `metrics`/`metrics_interval`, a `[metrics.scrape]` block, and a `max_samples_per_scrape` limit.

### 6.3 Alert Configuration (`[alerts]`)

```toml
[alerts]
# How often to evaluate the built-in rules (seconds). Default: 30.
# Rejected if zero (would panic tokio's interval timer at startup).
evaluation_interval_secs = 30

# Webhook / Slack / PagerDuty destinations. Repeatable.
[[alerts.destinations]]
type = "slack"                                   # "webhook" | "slack" | "pagerduty"
url = "https://hooks.slack.com/services/T/B/xxx"
severity = ["critical", "warning"]               # empty = all severities
# secret = "…"                                   # optional; HMAC key, or PagerDuty routing key

[[alerts.destinations]]
type = "pagerduty"
url = "https://events.pagerduty.com/v2/enqueue"
severity = ["critical"]
secret = "your-pagerduty-routing-key"
```

**Custom rules (`[[alert]]` with `expr`/`for`/`message`) and per-app suppression (`[app.*.alerts]`): Status: planned -- not yet implemented.** The five built-in threshold rules are fixed; there is no way to add, retune, or suppress rules from config today.

### 6.4 Aggregation Configuration

The push interval is `metrics.rollup_interval_secs` (§6.1). The consistent-hash assignment knob, per-tier rollup retention, and `max_rollup_size_bytes` from earlier drafts do **not** exist; `rollup_retention_hours` now prunes the rollup store on the disk-pressure tick (§3.3).

---

## 7. Failure Modes

### 7.1 Storage Exhaustion

**Trigger:** local metrics Parquet exceeds `max_storage_mb`, or `retention_days` elapses.

**Behaviour (`src/bun/disk_pressure.rs`):**

1. If `export_path` is set, un-exported Parquet files are shipped to the destination first (via `object_store`), and only files whose exact bytes are recorded in the export checkpoint become eligible for deletion.
2. Files are then pruned **oldest-first by mtime** when either they are past `retention_days` or total size exceeds `max_storage_mb`. With `max_storage_mb = 0` (the default) only the retention cutoff prunes.
3. Ingestion is never blocked; there is no in-memory ring-buffer degraded mode and no dedicated `mayo_storage_exhausted` alert.

**Recovery:** pruning is idempotent and runs each pressure check; once usage is back under the threshold nothing is deleted.

### 7.2 Aggregator Council Member Failure

The rollup worker keeps pushing to the last known parent and follows parent reassignment as membership changes. **The graceful-handoff details from earlier drafts -- consistent-hash reassignment, a 5-minute backfill on the new parent, and per-query gap annotations -- are Status: planned -- not yet implemented.** Today a reassignment simply starts fresh rollup history on the new parent.

### 7.3 Stale Rollups

**Status: planned -- not yet implemented.** The council rollup store does not track a per-node last-seen timestamp, does not mark nodes `stale`, does not exclude stale data from responses, and emits no `mayo_aggregator_stale_node` metric.

### 7.4 Scrape Target Timeout

**Status: planned -- not yet implemented.** There is no scraper in the running system (§5.2), so no scrape-timeout handling, backoff, or `mayo_scrape_timeout_total` counter exists.

### 7.5 Node Failure (Data Loss)

**Trigger:** A node crashes permanently, losing its local Mayo Parquet files.

**Behaviour:**

- All that node's local metric history is lost. This is an accepted trade-off of per-node storage.
- Its council parent retains the last rollups it received (a coarse 1-minute view), but because cluster-wide query fan-out is not wired (§5.5), that history is only visible through the parent that happens to hold it.

**Mitigation for teams requiring durability:** set `export_path` so Parquet files are shipped to object storage before pruning. Remote-read federation into external Prometheus/Thanos is **planned -- not yet implemented**.

---

## 8. Security Considerations

### 8.1 Metrics Access Control

- **Internal API:** the `/v1/metrics*` and `/v1/alerts` routes require an authenticated token (`AnyToken` in `src/bun/authz.rs`); inter-node transports are secured by the Sesame PKI where mTLS is enabled.
- **Per-namespace isolation:** `/v1/metrics/app/{app}/{namespace}` enforces tenant scope via `authorize_scoped`, and every path/name segment that reaches SQL is escaped (single quotes doubled), so a scoped token cannot read another tenant's metrics and a crafted `?name=` cannot break out of the string literal. This is **shipped**, not future.
- **Prometheus remote-read auth: Status: planned -- not yet implemented** (there is no remote-read endpoint, §3.4).
- **`max_samples_per_scrape` / label scrubbing:** planned -- not yet implemented (no scraper, §5.2).

### 8.2 Alert Webhook Authentication

- **HMAC signing (shipped):** when a destination has a `secret`, the body is signed HMAC-SHA256 and sent as `X-Mayo-Signature-256: sha256=<hex>`. For a PagerDuty destination the `secret` is the routing key.
- **TLS enforcement / `--allow-insecure-webhooks`: Status: planned -- not yet implemented.** HTTP webhook URLs are accepted; there is no scheme validation and no insecure-override flag. (Failure logs do redact the URL to scheme+host so a secret-bearing Slack path is not written to disk.)
- **Secret storage:** webhook secrets are read from the node's `[alerts.destinations]` config, held in memory by the dispatcher. They are **not** stored encrypted in Raft state.

---

## 9. Performance

**Reconciliation note:** the numbers in this section were derived for the planned Gorilla-encoded, tiered, scraping TSDB. The shipped store writes plain Parquet (Parquet's own column compression, no Gorilla), stores only the node + per-process metrics of §5.1 (no scraped, ingress, downsampled, or archived series), and has no remote-read or scrape path. Treat the tables below as design *targets* for the planned system, not measurements of what ships.

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

**Reconciliation note:** the subsections below describe tests for the planned system (tier transitions, aggregation accuracy, Prometheus/PromQL compatibility, scraping). The shipped tests instead cover: Parquet flush/reopen/prune (by data timestamp, not mtime), SQL-injection escaping, concurrent query-during-flush, corrupt-file skipping, the collector's metric names and labelling, the alert state machine (including stale-telemetry handling), and per-provider webhook serialisation. Read what follows as the target test matrix for the planned features, not the current suite.

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
| Data model | Prometheus labels+samples (universal) | name + label map + f64 sample, stored as Parquet columns | Simple, columnar, queryable with a standard engine |
| Query language | PromQL (Prometheus), InfluxQL (InfluxDB) | **DataFusion SQL** (not PromQL) | Reuse a mature embedded SQL engine over Parquet; no PromQL parser to build or maintain |
| Storage topology | Central server (Prometheus), sharded cluster (Cortex/VM) | Per-node, no central DB | Eliminates operational complexity; scales linearly; no SPOF |
| Cluster-wide queries | Scatter-gather all instances (Thanos), query sharded cluster (Cortex) | Hierarchical aggregation via council | Bounds fan-out at O(council_size) not O(cluster_size) |
| Deployment | Separate infrastructure (all) | Embedded in Bun binary | Zero operational overhead; batteries-included philosophy |
| Long-term retention | Object storage (Thanos, Cortex) | Local tiered downsampling + optional federation | No external storage dependency; federation available for teams that want it |

---

## 12. Libraries & Dependencies

### 12.1 Rust Crates

| Crate | Purpose | Notes |
|-------|---------|-------|
| `datafusion` | SQL query engine over Parquet | Executes queries against the on-disk Parquet blocks. Parquet read/write support ships with DataFusion, so there is no separate arrow/parquet dependency. Mayo's query language is DataFusion SQL, not PromQL. |
| `object_store` | Block storage abstraction | Reads and writes Parquet blocks behind a single interface over a local path, `file://`, `s3://`, or `gs://`. |
| `prometheus-parse` | Prometheus exposition parsing | Parses `text/plain` `/metrics` scrape responses (with `# HELP`, `# TYPE`, and metric lines) into samples. |
| `bincode` | Rollup serialisation | Encodes `NodeRollup` payloads pushed node-to-council over TCP. |
| `axum`/`hyper` | HTTP server | Serves the `/v1/metrics*` and `/v1/alerts` JSON endpoints. (No remote-read or `/metrics` exposition endpoint -- planned.) |
| `sysinfo` | Node + process metrics | Cross-platform CPU/memory/disk/network sampling; works on Linux and macOS without cgroup/`/proc` code. |
| `tokio` | Async runtime | Drives the scrape scheduler, query fan-out, rollup push, and compaction background tasks. Already used by Bun. |
| `reqwest` | HTTP client | Scrapes application `/metrics` endpoints and delivers webhook notifications. |

### 12.2 Build Considerations

All dependencies are compiled into the single Bun binary. There are no runtime dependencies on external libraries. The `sled`-vs-`rocksdb` label-index question is **moot**: there is no custom label index. DataFusion plans directly over the Parquet columns, and `object_store` abstracts local/`file://`/`s3://`/`gs://` storage behind one interface.

---

## 13. Open Questions

**Reconciliation note:** several questions below assume the planned Prometheus/PromQL/Gorilla design and are moot for what ships. There is no PromQL implementation (§13.1) -- the query language is DataFusion SQL and alerts are threshold rules; the remote-read/federation questions (§13.2) presuppose an endpoint that does not exist (§3.4); and the `sled`/`rocksdb` index-backend question (§13.4) is moot because there is no custom label index. They are retained as design history.

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
