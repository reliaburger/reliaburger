# Eyes Everywhere

In Chapter 6, we built a per-node metrics database. Arrow for columnar storage, DataFusion for SQL queries, Parquet for persistence. Every node collects system metrics, scrapes Prometheus endpoints, and stores everything locally. That works brilliantly for a single node. You query `localhost:9117/v1/metrics` and get exactly what you need.

But what happens when you have 500 nodes? Or 5,000? "Show me total CPU usage across the cluster" means hitting every single node's API, waiting for every response, and merging the results. That's O(N) fan-out. At scale, it's slow, fragile, and creates thundering-herd problems when someone refreshes the dashboard.

We need a way to answer cluster-wide questions without talking to every node. That's hierarchical metrics aggregation.

## The insight: partial aggregates

The trick is that most cluster-wide queries don't need raw data points. "Total CPU usage" needs a sum. "Peak memory" needs a max. "Average request latency" needs a sum and a count (to compute the average). You can pre-compute these aggregates on each node and ship the summaries instead of the raw samples.

A 1-minute window of CPU measurements might contain 6 samples (one every 10 seconds). Instead of shipping all 6 values, you ship one rollup: min=23.5, max=67.2, sum=285.3, count=6. From that rollup, the receiver can compute any standard aggregate without seeing the original data. The sum of sums is the total sum. The min of mins is the global min. The max of maxes is the global max. And the sum of sums divided by the sum of counts is the global average.

This is the same idea behind pre-aggregation in Prometheus recording rules and in Thanos/Cortex downsampling. We just bake it into the architecture.

## The two-tier tree

Here's the architecture. It mirrors the reporting tree from Chapter 7:

```
Worker nodes (hundreds or thousands)
    │
    │  Push 1-minute rollups every 60 seconds
    │  (deterministic assignment: hash(node_id) % council_size)
    ▼
Council members (3-7 nodes)
    │
    │  Store rollups in a separate RollupStore
    │  Answer cluster-wide queries from rollup data
    ▼
Query client (Brioche UI, relish CLI, API consumer)
    Fans out to council members only (3-7 requests, not 5,000)
```

The fan-out for a cluster-wide query is bounded by the council size, regardless of how many worker nodes exist. A 5-node council serves a 50-node cluster and a 5,000-node cluster with the same query latency.

## Rollup types

Let's start with the data model. A rollup captures the statistical summary of a metric over one time window:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RollupAggregate {
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    pub count: u32,
}
```

`Copy` is the right derive here. This is a 28-byte value type (three `f64`s and one `u32`). Passing it by value is cheaper than passing a reference. Rust's `Copy` trait marks types that can be duplicated by just copying bytes, no `clone()` needed. Numbers, booleans, and small structs of copyable fields qualify.

Each rollup entry preserves the metric identity, so the aggregator can answer label-filtered queries:

```rust
pub struct RollupEntry {
    pub metric_name: String,
    pub labels: BTreeMap<String, String>,
    pub aggregate: RollupAggregate,
}
```

And a `NodeRollup` bundles all entries from one node for one time window:

```rust
pub struct NodeRollup {
    pub node_id: NodeId,
    pub timestamp: u64,        // start of the 1-minute window
    pub entries: Vec<RollupEntry>,
}
```

A typical node running 10 apps produces roughly 20-30 rollup entries per minute (system metrics plus per-app metrics). The serialised `NodeRollup` fits comfortably under 5 KB. Even with 2,000 nodes pushing to a single council member, that's about 10 MB per minute. Easily manageable.

## Generating rollups

The `RollupGenerator` sits on each node. Every 60 seconds, it queries the local `MayoStore` for the previous minute's data using a GROUP BY query:

```rust
pub async fn generate(
    &self,
    store: &MayoStore,
    now: u64,
    extended: bool,
) -> Result<NodeRollup, MayoError> {
    let window = if extended {
        EXTENDED_ROLLUP_WINDOW_SECS  // 300 seconds
    } else {
        DEFAULT_ROLLUP_WINDOW_SECS   // 60 seconds
    };
    let start = now.saturating_sub(window);

    let aggregates = store.query_window_aggregates(start, now).await?;
    // ... map to RollupEntry structs
}
```

The `extended` flag matters during reassignment. More on that shortly.

Under the hood, `query_window_aggregates` runs this SQL through DataFusion:

```sql
SELECT metric_name, labels,
       MIN(value), MAX(value), SUM(value), COUNT(*)
FROM metrics
WHERE timestamp >= {start} AND timestamp < {end}
GROUP BY metric_name, labels
```

DataFusion handles the columnar aggregation efficiently. We're leveraging the same query engine from Chapter 6, just with a GROUP BY instead of a raw SELECT.

### A Rust pattern: `saturating_sub`

Notice `now.saturating_sub(window)` instead of `now - window`. In Rust, unsigned integer subtraction panics on underflow in debug builds and wraps in release builds. Neither is what you want. `saturating_sub` clamps the result to zero, which is the correct behaviour when `now` is smaller than the window size (for instance, just after the node started). This is one of those Rust habits that prevents subtle bugs: use saturating arithmetic for any subtraction on unsigned integers where the result might logically be negative.

## Storing rollups on the council

When a council member receives a `NodeRollup` (via the reporting transport), it ingests the data into a `RollupStore`. This is a separate store from the node's local `MayoStore`, with a different Arrow schema:

```
timestamp (u64) | node_id (utf8) | metric_name (utf8) | labels (utf8)
| min_val (f64) | max_val (f64) | sum_val (f64) | count_val (u32)
```

Eight columns instead of four. The extra columns capture the originating node and the aggregate statistics. The `RollupStore` follows the same buffer-flush-query pattern as `MayoStore`: insert into an in-memory buffer, periodically flush to a Parquet-backed RecordBatch, query via DataFusion SQL.

We could have reused `MayoStore` with synthetic metric names like `rollup_cpu_min`. But that would pollute the local metric namespace and make queries awkward. A separate store with its own schema keeps things clean. The Arrow/DataFusion infrastructure is cheap to instantiate -- the real cost is in the data, not the bookkeeping.

## Wiring into the reporting tree

The reporting tree already sends `StateReport` messages from workers to council members. We extend the `ReportingMessage` enum with one new variant:

```rust
pub enum ReportingMessage {
    Report(StateReport),
    Ack { node_id: NodeId },
    AggregatedReport { reports: HashMap<NodeId, StateReport> },
    MetricsRollup(NodeRollup),   // new
}
```

The `ReportAggregator` on each council member already handles `Report` and `AggregatedReport`. We add a case for `MetricsRollup`:

```rust
Some((_, ReportingMessage::MetricsRollup(rollup))) => {
    if let Some(ref store) = self.rollup_store {
        store.write().await.ingest(&rollup);
    }
}
```

That's it. One match arm. The existing transport handles framing, serialisation, and delivery. The existing assignment logic routes rollups to the right council member. No new transport layer needed.

## The RollupWorker

On each node, a `RollupWorker` runs as a separate spawned task with its own 60-second interval. It's separate from the `ReportWorker` (which sends `StateReport` every 1-5 seconds) because the data source, interval, and message type are all different. Combining them in one event loop would just add complexity.

The worker watches for council membership changes via a `watch` channel. When the assignment changes (because a council member joined or left), it sets a flag to send an extended rollup on the next tick, covering the previous 5 minutes instead of 1 minute. This backfills the new aggregator with enough historical data to answer queries immediately, rather than having a 5-minute gap.

```rust
fn update_parent(&mut self) {
    let council = self.council_rx.borrow().clone();
    let new_parent = assign_parent_address(&self.node_id, &council);

    if new_parent != self.parent_address {
        self.send_extended = true;
    }
    self.parent_address = new_parent;
}
```

## Two merge strategies

When results come back from multiple sources, how you combine them depends on the query type.

**Single-app queries** fan out to the 3-10 nodes running that app. Each node returns its own local data points. These might overlap (if a metric was reported by multiple sources), so we deduplicate by (timestamp, metric_name, labels):

```rust
pub fn merge_metrics_results(mut sources: Vec<Vec<MetricsQueryRow>>) -> Vec<MetricsQueryRow> {
    let mut all: Vec<MetricsQueryRow> = sources.drain(..).flatten().collect();
    all.sort_by_key(|r| r.timestamp);
    all.dedup_by(|a, b| {
        a.timestamp == b.timestamp
        && a.metric_name == b.metric_name
        && a.labels == b.labels
    });
    all
}
```

**Cluster-wide queries** fan out to the 3-7 council aggregators. Each returns a partial aggregate covering its subset of nodes. These must be *summed*, not deduplicated. If council member c1 reports `cpu_sum=30` (from workers w1 and w2) and c2 reports `cpu_sum=70` (from workers w3 and w4), the cluster total is 100, not 30 or 70.

```rust
pub fn merge_cluster_results(mut sources: Vec<Vec<MetricsQueryRow>>) -> Vec<MetricsQueryRow> {
    let mut sums: BTreeMap<(u64, String, String), f64> = BTreeMap::new();
    for source in sources.drain(..) {
        for row in source {
            let key = (row.timestamp, row.metric_name, row.labels);
            *sums.entry(key).or_default() += row.value;
        }
    }
    // ... convert back to rows
}
```

This distinction is easy to miss and causes subtle bugs if you get it wrong. The dedup merge produces correct results for single-app queries but silently drops data for cluster-wide queries. We discovered this during testing, which is exactly why you write tests first.

### `BTreeMap` for deterministic output

You might wonder why `merge_cluster_results` uses `BTreeMap` instead of `HashMap`. The `BTreeMap` keeps keys sorted, which means the output rows come out in a deterministic order (by timestamp, then metric name, then labels). A `HashMap` would produce correct sums but in an unpredictable order, making tests flaky and API responses inconsistent. When you need consistent ordering and your keys are comparable, `BTreeMap` is the right choice.

## The API endpoints

Three new endpoints expose the aggregation:

- `GET /v1/metrics/rollup` -- internal. Queried by other council members during fan-out. Returns raw rollup data from the local `RollupStore`.
- `GET /v1/metrics/cluster` -- cluster-wide query. Fans out to all council aggregators, sums partial results.
- `GET /v1/metrics/app/{app}/{namespace}` -- single-app query. Queries local metrics filtered by app labels.

The cluster endpoint returns a `MetricsQueryResult` with both data and warnings:

```rust
pub struct MetricsQueryResult {
    pub data: Vec<MetricsQueryRow>,
    pub warnings: Vec<QueryWarning>,
}

pub enum QueryWarning {
    NodeUnresponsive { node_id: String },
    DataUnavailable { node_id: String, from: u64, to: u64 },
}
```

Graceful degradation is the goal. If one council member is down, you get partial results plus a warning telling you which data is missing. No 500 error, no empty response, no silent data loss. The caller can decide whether partial data is good enough for their use case.

## Testing hierarchical aggregation

The roadmap requires a specific test: "node-level partial aggregates combine correctly at council level." Here's the key test:

```rust
async fn hierarchical_aggregation_correctness() {
    // Simulate 3 nodes with known CPU values
    let values = [10.0, 20.0, 30.0];

    // Generate rollups from each node's local MayoStore
    // Ingest all into a single council RollupStore
    // Query the RollupStore

    assert_eq!(agg.min, 10.0);   // min of mins
    assert_eq!(agg.max, 30.0);   // max of maxes
    assert_eq!(agg.sum, 60.0);   // sum of sums
    assert_eq!(agg.count, 3);    // sum of counts
    assert_eq!(agg.avg(), Some(20.0));  // sum / count
}
```

The integration test scales this up: 5 workers assigned to 3 council members, deterministic CPU values, verify the total across all council stores is exactly 150.0 (10+20+30+40+50). A second test simulates a downed aggregator and verifies that the partial sum from the remaining two council members is correct and consistent.

## Cross-node log queries

Metrics aggregate naturally -- a sum of sums is still a sum. Logs don't. You can't "aggregate" log lines; you can only collect them from everywhere and interleave them in the right order. That's what cross-node log queries do.

### The problem

When `relish logs web` runs, it needs to show logs from every instance of the "web" app across every node in the cluster. If web has 3 replicas on nodes 1, 3, and 7, we need to query all three and present a unified timeline.

### The approach

The leader (or any council member) receives the query and:

1. Looks up which nodes run the app from the Raft placement state
2. Fans out the query to those nodes in parallel
3. Each node queries its local LogStore and returns `Vec<LogEntry>` as JSON
4. The leader merge-sorts the results by timestamp
5. Returns the combined stream to the caller

This is simpler than the metrics case because there's no aggregation involved -- just concatenation and sorting. We already built `fan_out_query()` and `merge_log_entries()` back in Phase 6 and never wired them in. Now we do.

### Two endpoints, two purposes

We add two endpoints that work together:

`GET /v1/logs/entries/{app}/{namespace}` is the **internal** endpoint. It queries the local LogStore via DataFusion SQL and returns a JSON array of `LogEntry` objects. This is what `fan_out_query` calls on each node. It supports `start`, `end`, `grep`, and `tail` query parameters.

`GET /v1/logs/query/{app}/{namespace}` is the **cross-node** endpoint. It performs the full fan-out:

```rust
async fn logs_cross_node_handler(...) -> Response {
    // Look up placement from council
    let desired = council.desired_state().await;
    let app_id = AppId::new(&app, &namespace);
    let node_ids = desired.scheduling.get(&app_id)
        .map(|placements| placements.iter().map(|p| p.node_id.clone()).collect())
        .unwrap_or_default();

    // Resolve NodeIds to HTTP URLs via membership table
    let mut node_urls = Vec::new();
    for node_id in &node_ids {
        if let Some(info) = members.iter().find(|m| m.node_id == *node_id) {
            node_urls.push(format!("http://{}", info.address));
        }
    }

    // Fan out and merge
    let entries = fan_out_query(&log_query, &node_urls, &client, timeout).await?;

    // Apply tail after merge (important: each node applies its own tail,
    // but we want the global last-N after merging)
    if let Some(tail) = query.tail {
        if entries.len() > tail {
            entries = entries.split_off(entries.len() - tail);
        }
    }
}
```

### Why separate the existing endpoint

The original `/v1/logs/{app}/{namespace}` returns `{"logs": "multiline string"}`, which the CLI already depends on. The fan-out mechanism needs `Vec<LogEntry>` as JSON. Rather than breaking backward compatibility, we add `/v1/logs/entries/...` as the structured variant. Clean separation, no migration needed.

### Tail after merge

There's a subtle detail with `tail`. If you ask for `?tail=10` and the app runs on 3 nodes, each node returns its last 10 lines. After merge-sort, you have up to 30 lines. The cross-node handler applies `tail` again on the merged result, so the caller gets exactly the 10 most recent lines from the global timeline. The per-node tail is still useful -- it prevents each node from sending its entire log history when you only want the end.

### Graceful degradation

If a node is unreachable, the fan-out returns empty for that node. The response includes a `warnings` array listing which nodes didn't respond:

```json
{
  "entries": [...],
  "node_count": 3,
  "warnings": [{"NodeUnresponsive": {"node_id": "node-7"}}]
}
```

You get partial results rather than a hard failure. The caller decides whether that's acceptable.

### Testing cross-node queries

The integration tests spin up lightweight axum servers (one per simulated node), each backed by a LogStore with known entries at specific timestamps. Then they call `fan_out_query` and verify:

- Entries from all nodes appear in the result
- Results are sorted by timestamp
- Duplicate entries (same timestamp + line) are deduplicated
- Grep filtering works per-node before merge
- Unreachable nodes produce partial results, not errors

No Raft setup needed for these tests. The fan-out mechanism is the same regardless of whether the caller found the node URLs via Raft or a hardcoded list. We test the coordination layer in isolation.

## Log export and remote search

Logs stored on local disk eventually need to go somewhere more durable. A node might die, disks fill up, compliance requires retention. The original design said "export as jsonl.gz." We went a different direction: export as Parquet.

Why? Because we already produce Parquet files. The LogStore flushes its Arrow RecordBatches to Parquet every 60 seconds. Exporting means copying those files to a destination directory (local path, S3, GCS). No format conversion, no serialisation step, no gzip compression pipeline. Copy the bytes.

But the real payoff is on the read side. DataFusion can query Parquet files directly from any `object_store` backend. So `relish logs-search /tmp/backup/ "SELECT * FROM logs WHERE app='web'"` runs a full SQL query against exported archives, with predicate pushdown, without downloading the files first.

### The export engine

The exporter is deliberately simple:

```rust
pub fn export_logs(
    source_dir: &Path,
    destination: &str,
    node_id: &str,
    checkpoint: &mut ExportCheckpoint,
) -> Result<ExportResult, KetchupError>
```

It lists Parquet files in the source directory, skips any that have already been exported (tracked by an `ExportCheckpoint`), and copies the rest to `{destination}/{node_id}/`. The checkpoint is a JSON file persisted to disk, so export is incremental across restarts.

In the `bun` binary, the export runs as a spawned task on a configurable interval (default: 1 hour). The pattern is identical to the metrics collection and log flush tasks we've seen before.

### Remote search via DataFusion ListingTable

The search command creates a DataFusion `ListingTable` backed by the exported Parquet files:

```rust
let table_url = ListingTableUrl::parse(source_path)?;
let listing_options = ListingOptions::new(Arc::new(ParquetFormat::default()))
    .with_file_extension(".parquet");
let config = ListingTableConfig::new(table_url)
    .with_listing_options(listing_options)
    .with_schema(Arc::new(log_schema()));
let table = ListingTable::try_new(config)?;
ctx.register_table("logs", Arc::new(table))?;
```

`ListingTable` tells DataFusion "there's a directory of Parquet files, treat them all as one table." DataFusion handles the rest: listing files, reading row groups, applying predicate pushdown, columnar filtering. You get the full SQL engine over your log archives for free.

The `ListingTableUrl` abstraction is what makes this work with both local paths and remote URLs. With the `fs` feature of `object_store`, local paths work out of the box. Adding `aws` or `gcp` features would enable `s3://` and `gs://` URLs with no code changes.

### Two CLI commands

`relish logs-export --dest /tmp/backup/` copies local Parquet files to a destination. It finds the LogStore directory, loads (or creates) a checkpoint, exports new files, and reports results.

`relish logs-search /tmp/backup/node-1/ "SELECT app, COUNT(*) FROM logs GROUP BY app"` runs SQL directly against the exported files. No running agent needed. DataFusion reads Parquet, executes the query, and prints JSON results. Aggregations, joins, window functions, LIKE patterns -- the full SQL dialect.

## Disk pressure: export before you delete

There's a subtle problem with scheduled export. The export runs every hour. Pruning runs separately based on retention days or storage limits. What if the disk fills up between exports? You lose data that was never backed up.

The solution is to tie these two operations together. Before deleting anything, make sure it's been exported first.

The `check_and_relieve` function runs every 5 minutes and does exactly this:

```rust
pub fn check_and_relieve(
    source_dir: &Path,
    export_dest: Option<&str>,
    node_id: &str,
    checkpoint: &mut ExportCheckpoint,
    max_bytes: u64,
    retention_days: u32,
) -> PressureResult
```

When the Parquet directory exceeds `max_bytes`, it:

1. Exports any un-exported files to the configured destination
2. Collects all Parquet files sorted oldest-first
3. For each file that's past retention OR over the size limit, checks whether it's been exported
4. Only deletes files that have been safely exported (or that have no export destination configured)
5. Stops once usage is under the threshold

The key invariant: **files are never deleted locally until they've been successfully exported.** If the export destination is unreachable, the disk fills up — which is the correct behaviour. You'd rather run out of disk space (which triggers alerts) than silently lose data.

This applies to both logs and metrics. The `bun` binary spawns one disk pressure task that checks both directories. Configuration is per-section:

```toml
[logs]
max_storage_mb = 500    # prune exported log files when exceeding 500 MB
export_path = "/mnt/backup/logs/"

[metrics]
max_storage_mb = 200    # prune exported metrics files when exceeding 200 MB
export_path = "/mnt/backup/metrics/"
```

Setting `max_storage_mb = 0` (the default) disables size-based pruning. Retention-based pruning (via `retention_days`) still applies. You can use both: retention handles the steady state, max_storage handles spikes.

## Upgrading Brioche: from meta-refresh to HTMX

Back in Chapter 6, we built the Brioche dashboard as a single page. Format strings, inline CSS, and a `<meta http-equiv="refresh" content="5">` tag that reloaded the entire page every five seconds. It worked, but it was blunt. Every reload flashed the screen, reset scroll position, and re-fetched everything whether it changed or not.

Now we need more pages — app detail, node detail — and the old approach doesn't scale. So we're introducing two small libraries: HTMX for partial page updates and uPlot for time-series charts. No React, no Vue, no build pipeline. `cargo build` still produces everything.

### Why not Askama?

The design doc specifies Askama templates — compile-time type-checked HTML rendering. It's a good idea when you have 10+ pages. We have three. Format strings got us this far and they'll keep working. Each page is a self-contained render function in its own file under `src/brioche/`. If the page count grows, we can migrate to Askama without touching anything outside `src/brioche/`.

### HTMX for partial updates

The key insight is that you don't need a JavaScript framework to update part of a page. HTMX adds a few HTML attributes and the browser does the rest:

```html
<div hx-get="/ui/fragment/apps"
     hx-trigger="every 5s"
     hx-swap="innerHTML">
  <!-- Server-rendered table goes here -->
</div>
```

Every 5 seconds, HTMX fires a GET request to `/ui/fragment/apps`, receives a bare `<table>` fragment (no `<html>` wrapper), and swaps it into the `<div>`. The rest of the page stays put. No flashing, no scroll reset, no wasted bandwidth.

We split rendering into two layers:

1. **Full page renders** (`render_dashboard`, `render_app_detail`, `render_node_detail`) produce complete HTML documents with `<head>`, `<body>`, and all the script/CSS links.
2. **Fragment renders** (`render_apps_table_fragment`, `render_instance_table_fragment`, etc.) produce bare HTML that HTMX can swap in. The full-page renders call these internally, so the initial page load and subsequent HTMX updates use the same rendering code.

### uPlot for charts

uPlot is a 10KB JavaScript library that renders time-series charts on a canvas element. It handles millions of data points at 60fps — massively overkill for our use case, but that means it'll never be the bottleneck.

The server doesn't know about uPlot. It renders a `<div>` with a JSON `data-chart-config` attribute:

```html
<div data-chart-config='{"endpoint":"/v1/metrics/app/web/default?name=process_cpu_percent",
                          "title":"CPU Usage","y_label":"%",
                          "refresh_secs":10,"range_secs":3600}'>
</div>
```

A small custom script (`brioche.js`, about 100 lines) finds these elements on page load, creates uPlot instances, and periodically fetches data from the existing metrics API. The metrics endpoints already return JSON arrays of `{timestamp, value}` objects — no new backend work needed.

### Vendored assets, no build pipeline

HTMX and uPlot ship as single minified JS files. We vendor them into `brioche/dist/` and embed them into the binary via `rust-embed`:

```rust
#[derive(rust_embed::Embed)]
#[folder = "brioche/dist/"]
struct BriocheAssets;
```

At runtime, `GET /ui/static/htmx.min.js` serves the file from the binary's memory. No filesystem reads, no CDN dependency, no separate install step. Total JS payload: ~50KB (HTMX) + ~50KB (uPlot) + ~3KB (custom) — about 103KB uncompressed. For comparison, Grafana loads 2-5MB of JavaScript.

### App detail page

Navigate to `/ui/app/web/default` and you get a page with:

- **Header**: app name, namespace, state, instance count
- **Charts**: CPU and memory usage over the last hour, auto-refreshing every 10 seconds
- **Instance table**: each running instance with state, restarts, port, PID — auto-refreshing via HTMX
- **Streaming logs**: an SSE connection to the existing log follow endpoint, appending lines in real time
- **Deploy history**: past deployments with image, result, and step counts
- **Environment variables**: every env var from the app spec, with encrypted values displayed as `[encrypted]`

That last point is important. The `EnvValue` type has `Plain` and `Encrypted` variants, and the `Serialize` impl outputs the raw ciphertext. We can't use that — it would send `ENC[AGE:longbase64...]` to the browser.

Instead, we define a `SafeEnvValue` type that replaces encrypted values before serialisation:

```rust
pub fn safe_env(env: &BTreeMap<String, EnvValue>) -> Vec<SafeEnvValue> {
    env.iter()
        .map(|(k, v)| SafeEnvValue {
            key: k.clone(),
            value: match v {
                EnvValue::Plain(s) => s.clone(),
                EnvValue::Encrypted(_) => "[encrypted]".to_string(),
            },
            encrypted: v.is_encrypted(),
        })
        .collect()
}
```

A test verifies the invariant: serialise the output to JSON and assert it never contains `ENC[AGE:`. The masking happens at the API layer, not the UI layer — even a direct `curl` to the env endpoint gets masked values.

### Node detail page

Navigate to `/ui/node/node-01` for per-node resource charts (system CPU and memory from Mayo) and a table of all running instances on that node. App names link back to their detail pages.

### Storing deployed specs

To serve environment variables, the API needs the original `AppSpec` after deployment. The agent previously discarded configs once instances were running. We added a `deployed_specs: HashMap<(String, String), AppSpec>` field to `BunAgent` and populate it during deploy. A new `AgentCommand::AppConfig` variant retrieves it for the env endpoint.

## Alert webhooks

In Chapter 6, we built the alert state machine: five rules, three states (Inactive → Pending → Firing), and a `firing_alerts()` method for the API. But we never actually drove it. The evaluator sat there, idle, waiting for someone to call `evaluate()`. Now we wire it up and add webhook delivery.

### The evaluation loop

A new background task in `bun.rs` runs every 30 seconds (configurable via `alerts.evaluation_interval_secs`). It:

1. Reads the latest metric values from MayoStore (last 120 seconds, DESC by timestamp, first value per metric wins)
2. Computes derived percentage metrics (`node_memory_usage_percent`, `node_disk_usage_percent`) from the raw byte values the collector produces
3. Calls `evaluator.evaluate(latest_values)`, which now returns a `Vec<AlertTransition>`
4. For each transition, spawns a fire-and-forget webhook delivery task

That last point matters. If a webhook endpoint is slow or down, the retry loop (1s → 5s → 25s) takes 31 seconds total. We can't block the next evaluation tick waiting for that. So each dispatch runs in its own `tokio::spawn`. The evaluation loop moves on immediately.

### Detecting transitions

The original `evaluate()` updated state in-place and returned nothing. We needed to know *what changed*: which alerts just started firing, which just resolved. So we modified `evaluate()` to snapshot the previous states, compare after the update, and return a `Vec<AlertTransition>`:

```rust
let was_firing = matches!(prev_state, AlertState::Firing { .. });
let now_firing = matches!(new_state, AlertState::Firing { .. });

if now_firing && !was_firing {
    transitions.push(AlertTransition { kind: TransitionKind::Firing, .. });
} else if was_firing && !now_firing {
    transitions.push(AlertTransition { kind: TransitionKind::Resolved, .. });
}
```

Resolved notifications are just as important as firing ones. An operator who gets paged about high memory usage wants to know when it recovers, without having to check the dashboard.

### The webhook payload

Every destination receives the same JSON structure:

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
  "timestamp": 1700000030
}
```

We deliberately don't format this for Slack or PagerDuty specifically. A generic webhook endpoint can parse this JSON and do whatever it needs. Slack's incoming webhooks expect a `text` field -- the receiver can transform the generic payload into that format. This keeps the Reliaburger side simple and lets operators adapt the integration to their workflow.

### HMAC signing

When a destination has a `secret` configured, we sign the payload body with HMAC-SHA256 and include the signature in the `X-Mayo-Signature-256` header:

```rust
let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
let tag = hmac::sign(&key, body);
format!("sha256={}", hex::encode(tag.as_ref()))
```

This is the same pattern we use in `lettuce/webhook.rs` for verifying incoming Git webhooks, just running in the opposite direction. The receiver computes the same HMAC over the request body with the shared secret and compares. If the signatures match, the payload is authentic.

### Retry with backoff

Failed deliveries get three attempts: 1 second, 5 seconds, 25 seconds. After three failures, the notification is dropped and logged. We considered a queue with persistent retries, but that adds complexity for diminishing returns. If your webhook endpoint is down for 31 seconds, you probably have bigger problems -- and the next evaluation cycle will fire the same alert again if it's still active.

### Configuration

```toml
[alerts]
evaluation_interval_secs = 15

[[alerts.destinations]]
type = "webhook"
url = "https://hooks.slack.com/services/T/B/xxx"
severity = ["critical", "warning"]

[[alerts.destinations]]
type = "webhook"
url = "https://events.pagerduty.com/v2/enqueue"
severity = ["critical"]
secret = "my-shared-secret"
```

Empty `severity` means all alerts. Each destination can filter independently. PagerDuty gets only critical alerts; Slack gets everything.

## What's next

Phase 11 is complete. We have cluster-wide metrics via hierarchical aggregation, cross-node log queries, Parquet log export with remote search, disk pressure management, a multi-page Brioche UI with HTMX and uPlot charts, and alert webhooks with HMAC signing and retry.

PromQL-to-SQL translation is deferred to v2. The SQL interface works well enough for now, and building a correct PromQL translator is a project in itself. Better to ship what works and add compatibility later.
