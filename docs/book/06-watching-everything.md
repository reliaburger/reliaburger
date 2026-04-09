# Watching Everything

You can build the most reliable container orchestrator in the world, but if you can't see what it's doing, you're flying blind. Phase 6 adds observability: metrics collection, log capture, alerting, and a dashboard. All built in. No Prometheus server to deploy, no Elasticsearch cluster to manage, no Grafana to configure.

## Why not just use Prometheus?

Prometheus is excellent. It's the industry standard for metrics. But it's also a separate system: you deploy it, configure scraping targets, set up alerting rules, run Alertmanager for notifications, deploy Grafana for dashboards, and manage all their storage. That's four more services to keep running, each with its own failure modes.

Reliaburger takes a different approach. The metrics database, log collector, alert evaluator, and dashboard are compiled into the same `bun` binary that runs your containers. When the node starts, observability starts. When the node stops, it stops. No separate lifecycle to manage.

## Standing on the shoulders of InfluxDB

We could have written a custom time-series database from scratch. Gorilla XOR compression, WAL segments, compaction, the whole thing. It would have taken months and introduced subtle correctness bugs that take years to shake out.

Instead, we reuse the same building blocks that power InfluxDB 3.0, DeltaLake, and Apache Iceberg:

- **Arrow** for columnar in-memory storage
- **DataFusion** for SQL queries
- **Parquet** for on-disk persistence
- **object_store** for storage abstraction (local disk or S3)

This gives us a production-grade metrics engine in a few hundred lines of glue code. The heavy lifting — columnar compression, predicate pushdown, vectorised execution — is handled by libraries that thousands of engineers have battle-tested.

## SQL, not PromQL

Here's a controversial choice. Prometheus uses PromQL, a purpose-built query language for time-series data. It's powerful, but it confuses people. Even experienced engineers struggle with the difference between `rate()` and `irate()`, with range vectors versus instant vectors, with the `offset` modifier. PromQL is a language you have to learn, and most people learn just enough to copy-paste from Stack Overflow.

Our metrics use SQL:

```sql
SELECT timestamp, metric_name, value
FROM metrics
WHERE metric_name = 'node_cpu_usage_percent'
AND timestamp > 1704067200
ORDER BY timestamp
```

If you know SQL, you already know how to query our metrics. No new DSL to learn. DataFusion gives us the full SQL engine — aggregations, joins, subqueries, window functions — for free.

Could we add PromQL support later? Yes. A translator covering the 20% of PromQL that people actually use — `rate()`, `sum by()`, `avg by()`, `histogram_quantile()`, comparison operators — would let existing Grafana dashboards work without rewriting queries. That's a Phase 9 job.

## The storage abstraction

Here's where it gets interesting. The `object_store` crate abstracts over local filesystems, S3, GCS, and Azure Blob Storage. DataFusion reads Parquet files from any of them transparently.

On your dev laptop, metrics write to `~/.local/share/reliaburger/metrics/` as Parquet files. In production, you set one config field:

```toml
[metrics]
object_store_url = "s3://my-bucket/reliaburger-metrics"
```

Same code, same queries, same dashboard. The only difference is where the bytes go. Your metrics survive node failures because they're in S3, not on a local disk that just caught fire.

## Collecting metrics

The `sysinfo` crate gives us cross-platform system metrics without writing platform-specific code. On both Linux and macOS, we collect:

- **Node-level:** CPU usage, memory used/total, disk used/total, network rx/tx bytes and packets
- **Per-process:** CPU percentage and RSS memory for each running container (by PID)

Collection runs every 10 seconds. Each sample is a `(timestamp, metric_name, labels_json, value)` tuple, stored as an Arrow RecordBatch. When the batch fills up, it's flushed to a Parquet file.

## Prometheus scraping

Not everything comes from system stats. Your apps might expose custom metrics via a `/metrics` endpoint in the Prometheus text format. Reliaburger scrapes these automatically.

The `prometheus-parse` crate handles the parsing. When an app has a health check configured, we probe `/metrics` on the same port. If it responds with valid Prometheus text, we ingest it alongside the system metrics. Same Arrow schema, same SQL queries.

## Alert evaluation

Five built-in alert rules catch the most common failure modes:

1. **cpu_throttle** — CPU above 90% for 5 minutes (critical)
2. **oom_risk** — memory above 85% for 2 minutes (critical)
3. **memory_high** — memory above 70% for 10 minutes (warning)
4. **disk_high** — disk above 80% for 5 minutes (warning)
5. **cpu_idle** — CPU below 5% for 30 minutes (warning, possible zombie)

The alert state machine is simple: Inactive → Pending → Firing. A metric breaches its threshold, the alert goes to Pending. If the breach persists for the required duration, it fires. If the metric recovers, the alert goes back to Inactive. No hysteresis, no complex inhibition rules. Just thresholds.

## Ketchup: where the logs go

Every line that a container writes to stdout or stderr ends up in Ketchup's append-only log files. One file per app per day, stored under `{logs_dir}/{namespace}/{app}/{date}.log`.

Each log line is prefixed with a timestamp and stream indicator:

```
1704067200 O starting up
1704067201 E warning: config file not found, using defaults
1704067202 O listening on :8080
```

`O` for stdout, `E` for stderr. Simple, grep-friendly, human-readable.

A sparse timestamp index sits alongside each log file. Every 4KB of log data, we record `(byte_offset, timestamp)`. To find logs from the last hour, binary search the index for the start timestamp, seek to that offset, and scan forward. No need to read the entire file.

JSON auto-detection examines the first 10 lines. If they parse as JSON objects, the stream is marked as structured, enabling field-level queries:

```bash
relish logs api --json-field level=error
```

## SQL over logs

Here's something you don't see in most observability stacks: the same SQL engine that queries your metrics also queries your logs.

Ketchup stores logs in the same Arrow/DataFusion/Parquet stack that Mayo uses for metrics. The schema is five columns: `timestamp`, `app`, `namespace`, `stream`, and `line`. Want to find all errors from the web app in the last hour?

```sql
SELECT timestamp, line FROM logs
WHERE app = 'web'
AND timestamp > 1704067200
AND line LIKE '%ERROR%'
ORDER BY timestamp
```

No new query language. No log-specific DSL. Just SQL.

### Why columnar storage works for logs

You might think logs are just text, so columnar storage wouldn't help. But most of the data in a log line isn't the message — it's the metadata. The `app` column for 10,000 lines from the same app stores "web" once in a dictionary and references it 10,000 times. The `namespace` and `stream` columns work the same way. Timestamps delta-encode beautifully.

Even the `line` column compresses well. If your app is stuck in an error loop printing the same stack trace 10,000 times, Parquet's dictionary encoding stores it once. An error loop that eats 2MB as flat text might be 10KB in Parquet.

Overall, expect 3-5x compression versus flat log files.

### How LIKE queries work without full-text indexes

When you write `WHERE line LIKE '%ERROR%'`, DataFusion doesn't have an inverted index to consult. It scans the `line` column. But columnar storage makes this much faster than grep on a flat file:

1. **Columnar pruning.** DataFusion only reads the `line` column, not timestamp/app/namespace/stream. That alone can skip 60% of the data.

2. **Predicate pushdown.** A query like `WHERE app = 'web' AND timestamp > X AND line LIKE '%ERROR%'` filters by app first (dictionary lookup, instant), then by timestamp (range check), and only scans `line` for the surviving rows. If 99% of rows are eliminated before the LIKE, the scan is tiny.

3. **Row group statistics.** Parquet files are split into row groups. Each group stores min/max values per column. A time-range query can skip entire groups without reading them.

This isn't a full-text search engine. If you need to search millions of unique log lines by arbitrary substring, you'd want something like Elasticsearch. But for the common case — filter by app and time first, then grep — it's fast.

A future improvement: Parquet supports bloom filters per column. Writing a bloom filter on the `line` column during flush would let DataFusion skip row groups that definitely don't contain the search term.

### The unified query path

Both the flushed Parquet files and the unflushed in-memory buffer are included in every DataFusion query. Same trick we use for metrics. There's no blind spot — you see logs from 30 seconds ago in the same SQL query as logs from last week. No merging, no separate code paths, no seams.

## The dashboard

Brioche is a single HTML page. No React, no Vue, no webpack. The server renders the HTML with current data, embeds a 2KB CSS stylesheet, and sends it. The browser refreshes every 5 seconds via a `<meta http-equiv="refresh">` tag.

The dashboard shows three sections: apps (name, status, instance count), nodes (name, state, app count), and alerts. Status dots are green for healthy, amber for pending, red for failed. The dark theme is easy on the eyes during those late-night debugging sessions.

Total payload: under 10KB. First paint: instant.

## Under the hood: key patterns

### Arrow RecordBatch construction

Each metrics sample starts as a Rust struct. To get it into DataFusion, we transpose the data into columnar arrays and wrap them in a `RecordBatch`:

```rust
fn buffer_to_batch(&self) -> Result<Option<RecordBatch>, MayoError> {
    if self.buffer.is_empty() {
        return Ok(None);
    }

    let timestamps: Vec<u64> = self.buffer.iter().map(|s| s.timestamp).collect();
    let names: Vec<&str> = self.buffer.iter().map(|s| s.metric_name.as_str()).collect();
    let values: Vec<f64> = self.buffer.iter().map(|s| s.value).collect();

    let batch = RecordBatch::try_new(
        Arc::new(metrics_schema()),
        vec![
            Arc::new(UInt64Array::from(timestamps)),
            Arc::new(StringArray::from(names)),
            Arc::new(Float64Array::from(values)),
        ],
    )?;
    Ok(Some(batch))
}
```

Four iterations over the same buffer, producing four column vectors. If you're coming from Python, think of it as converting a list of dicts into a dict of lists — the same data, rotated 90 degrees. Each column becomes an `Arc<dyn Array>` because DataFusion needs shared ownership (multiple query operators might read the same batch concurrently).

The `?` on `try_new` catches schema mismatches: if you pass three arrays when the schema expects four, you get an error at batch construction time, not somewhere deep in a query plan. Fail fast.

### The alert state machine

The alert evaluator has three states and four transitions, all in a single `match`:

```rust
let new_state = match (&state, breaching) {
    (AlertState::Inactive, true) => AlertState::Pending { since: now },
    (AlertState::Pending { since }, true) => {
        if now.duration_since(*since).unwrap_or_default() >= rule.for_duration {
            AlertState::Firing { since: *since }
        } else {
            state.clone()
        }
    }
    (AlertState::Firing { .. }, true) => state.clone(),
    (_, false) => AlertState::Inactive,
};
```

Four arms cover every case. The `since` field is set when the alert enters Pending and preserved when it moves to Firing — so you know when the breach *started*, not when it was confirmed. The wildcard `(_, false)` handles the recovery case: no matter what state you're in, if the metric is no longer breaching, go back to Inactive. No hysteresis, no debounce. Simple.

If you're used to state machines in Go or Java, this might look too compact. Where are the separate `handleInactive()`, `handlePending()`, `handleFiring()` methods? Rust's pattern matching lets you collapse them into one expression. The compiler ensures you handle every combination — add a fourth state and every `match` in the codebase that doesn't handle it becomes a compilation error.

### Sparse indexing: the write path

The sparse index update on log append is the kind of trick that's easy to get wrong. We only write an index entry when we cross a 4KB boundary:

```rust
let offset_after = offset_before + record.len() as u64;
if offset_before / INDEX_INTERVAL != offset_after / INDEX_INTERVAL {
    index.add(offset_before, timestamp);
    index.write_to(&idx_path)?;
}
```

Integer division does the heavy lifting. If both offsets are in the same 4KB block, the division produces the same result and we skip the index update. If they straddle a boundary, we record the offset. One comparison, no modular arithmetic, no counters to maintain.

The cost: for a 100MB log file, the sparse index has about 25,000 entries (one per 4KB). Binary search finds any timestamp in ~15 comparisons. Sequential scan from there covers at most 4KB of log data. The combination gives us O(log n) time-range queries without maintaining a full index.

## What we learned

### Reuse the query engine, don't build one

DataFusion gives us SQL parsing, query planning, columnar execution, predicate pushdown, and Parquet I/O. That's roughly 200,000 lines of code we didn't write. Our glue layer is about 400 lines. The ratio (500:1) is the best leverage in the entire project.

The temptation was to build something simpler: a custom iterator over Parquet files with hardcoded filters. It would have been "enough" for v1. But then you want time-range queries, then aggregations, then LIKE filters, then JSON field extraction, and suddenly you've built half a query engine badly. Start with DataFusion and you skip the reinvention.

### Five default alerts cover 90% of incidents

We thought operators would want to define custom alert rules from day one. In practice, the five defaults (CPU throttle, OOM risk, memory high, disk high, CPU idle) catch nearly every production incident that metrics can detect. Custom rules are a Phase 9 feature, and nobody has complained about the delay.

The lesson: don't build config for things that have obvious defaults. Ship the defaults, add config later if someone needs it.

### Server-rendered HTML with meta refresh beats React

The Brioche dashboard is a single server-rendered HTML page. No JavaScript framework, no API calls, no state management. The browser refreshes every 5 seconds. Total payload: 10KB. Time to first meaningful paint: zero seconds (it's all in the HTML response).

Could we build a nicer dashboard with React and WebSocket updates? Sure. But that's a separate build pipeline, a node_modules tree, a bundler, and an entire frontend ecosystem to maintain. The server-rendered approach gives us something that works today and costs nothing to maintain.

## Test count

Phase 6 adds 120 tests, bringing the total to 991. The new tests cover Arrow schema validation, DataFusion SQL queries over both metrics and logs, Parquet persistence, system metrics collection (CPU, memory, disk, network), Prometheus text parsing, alert state machine transitions, log append/query/grep/tail/JSON filtering, LogStore SQL queries (app filter, time range, LIKE grep, LIMIT), sparse index binary search, cross-node log merge, and dashboard HTML rendering.
