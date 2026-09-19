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

Could we add PromQL support later? Yes. A translator covering the 20% of PromQL that people actually use — `rate()`, `sum by()`, `avg by()`, `histogram_quantile()`, comparison operators — would let existing Grafana dashboards work without rewriting queries. That's a Phase 11 job.

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

The `prometheus-parse` crate handles the parsing. Configure `(job, url)` targets in `[metrics]` using `scrape_targets`; Bun's scrape task calls `scrape_once` at `scrape_interval_secs`. A health check does not automatically register a scrape target. Valid samples enter the same Arrow schema and SQL queries as system metrics. An empty target list starts no scrape task.

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

The alert evaluator has three states, all decided in a single `match`. The first version matched on a boolean `breaching` flag. That had a bug we'll come back to, so here's the version we actually ship, which matches on the metric value itself (an `Option<f64>`):

```rust
let new_state = match (&prev_state, value) {
    // No data at all: keep firing (can't prove recovery), else go inactive.
    (_, None) => match &prev_state {
        AlertState::Firing { .. } => prev_state.clone(),
        _ => AlertState::Inactive,
    },
    (AlertState::Inactive, Some(v)) if rule.operator.eval(v, rule.threshold) => {
        AlertState::Pending { since: now }
    }
    (AlertState::Pending { since }, Some(v)) if rule.operator.eval(v, rule.threshold) => {
        if now.duration_since(*since).unwrap_or_default() >= rule.for_duration {
            AlertState::Firing { since: *since }
        } else {
            prev_state.clone()
        }
    }
    (AlertState::Firing { .. }, Some(v)) if rule.operator.eval(v, rule.threshold) => {
        prev_state.clone()
    }
    // Data present and back in range: a genuine recovery.
    (_, Some(_)) => AlertState::Inactive,
};
```

The `since` field is set when the alert enters Pending and preserved when it moves to Firing, so you know when the breach *started*, not when it was confirmed. The `if rule.operator.eval(...)` bits are *match guards*: a `match` arm only fires when its pattern matches *and* the guard is true. Rust checks that the arms are still exhaustive with guards in place, so nothing slips through.

If you're used to state machines in Go or Java, this might look too compact. Where are the separate `handleInactive()`, `handlePending()`, `handleFiring()` methods? Rust's pattern matching collapses them into one expression, and the compiler ensures you handle every combination. Add a fourth state and every `match` in the codebase that doesn't handle it becomes a compilation error.

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

## Hardening the metrics path

The first cut of Mayo worked in the demo and passed its tests. A later review found five sharp edges that only bite in production, not in a thirty-second demo. They're worth walking through, because each one is a small change that fixes a whole class of failure.

**SQL injection through a metric name.** The per-app query endpoint built its SQL by pasting the caller's `?name=` and the app's `namespace/app` straight into the string. Send `?name=x' OR '1'='1` and the injected quote closes the literal early, drops the tenant and time predicates, and hands back every app's metrics. The fix is the same one every database driver ships: escape the value. DataFusion follows standard SQL, so a `'` inside a literal is doubled:

```rust
pub(crate) fn escape_sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}
```

Now `x' OR '1'='1` is matched *literally*, finds no metric by that name, and returns nothing. We escape every interpolated value, including the app and namespace, not just the obvious one.

**Rollups that double-count.** Each worker rolls up its last minute of metrics and pushes the summary to a council member. After a reassignment a worker re-sends its recent windows as backfill, so the same `(node, window)` can arrive twice. The council was summing both, inflating the cluster total. Two changes fix it. First, we align every window to a minute boundary (`now - (now % 60)`), so two nodes ticking a few seconds apart stamp the same minute with the same timestamp. Second, we make ingest idempotent, keyed on `(node_id, window)`:

```rust
let key = (node_id.clone(), rollup.timestamp);
if !self.seen_windows.insert(key) {
    return false; // already ingested this window; drop it
}
```

`HashSet::insert` returns `false` if the key was already present, which is exactly the "have I seen this?" question we need in one call.

**Rollups that vanished on restart.** The rollup store kept its flushed data in an in-memory `Vec<RecordBatch>` and named every Parquet file with a counter that reset to zero on start. So a restart both lost all history *and* overwrote `rollup_000000.parquet` with new data. We fixed both by making the rollup store read its history back from the Parquet directory (like the metrics store already did) and by seeding the flush counter one past the highest file on disk. Restart now recovers everything and appends rather than clobbering.

**Per-app metrics that were never collected.** Production only collected node-level metrics (CPU, memory for the whole box). The autoscaler and the per-app dashboards had nothing to read. The collector already knew how to scrape a single process; it just wasn't being called. The collection loop now asks the agent for its running instances and collects per-process CPU and memory for each one, labelled `namespace/app`.

**A flush that froze every query.** The flush wrote Parquet while holding the store's write lock, and Arrow's writer is synchronous. So for the duration of the write, every query waited. Worse, blocking I/O on an async task stalls the whole tokio runtime. We split the flush in two: drain the buffer under a brief lock, then write outside it, on the blocking pool:

```rust
let pending = { store.write().await.take_flush_batch()? };  // brief lock
if let Some(p) = pending {
    write_pending_flush(p).await?;  // no lock held; runs on spawn_blocking
}
```

While the write is in flight, queries hold a read lock and proceed. And a corrupt or truncated Parquet file (a flush killed mid-write) no longer poisons the directory: we read each file on its own and skip the bad one with a log, so one botched flush doesn't fail every unrelated read.

The theme across all five: the happy path was fine, and the failure paths — an attacker, a reassignment, a restart, a dead app, a crash mid-flush — were where the bugs lived. That's usually where they live.

## Hardening the log path

Ketchup had the same shape of problem: a demo-clean happy path and a set of failure paths nobody had walked. The same review found five.

**A raw-SQL endpoint with no seatbelt.** `GET /v1/logs/sql?q=…` handed the caller's SQL straight to DataFusion. That's a lot of rope: `SELECT * FROM logs` streams the whole archive back through one response, an unbounded aggregation can exhaust the agent's memory, and a `DROP`/`INSERT` shouldn't be reachable from a read endpoint at all. The bounded path fixes all three. It accepts only a read-only `SELECT`/`WITH`, runs against a session that registers just the `logs` table (so a reference to any other table fails to plan rather than reading it), wraps the query in an outer `LIMIT` to cap rows, and runs under a working-memory limit so a runaway sort *errors* instead of taking the node down:

```rust
let bounded = format!("SELECT * FROM ({trimmed}) AS bounded LIMIT {MAX_LOG_SQL_ROWS}");
```

A rejected query is a `400`, not a `500` — the client asked for something the endpoint won't do, and the error says which.

**Fan-out that hid the dead.** When an app runs on several nodes, the leader asks each node for its slice of the logs and merges the answers. The first version turned *every* failure — a node down, a non-2xx status, unparseable JSON, a panicked task — into an empty success. So "this app produced no logs" and "half your cluster is unreachable" looked identical. Now fan-out returns a partial result: the entries it *did* collect, plus a list of which nodes failed and why. The caller can tell the two apart.

**A merge that deleted real events and kept fakes.** The merge deduplicated only *adjacent* equal `(timestamp, line)` pairs after sorting. Two problems. If a third line at the same timestamp sorted between two genuine duplicates, they weren't adjacent and both survived. And two replicas that each logged an identical line at the same instant — two real, distinct events — sorted adjacent and got collapsed into one. The fix is to dedup on a stable identity that includes *which node* produced the line: `(node, timestamp, stream, line)`. The same event reported twice by one node collapses; the same line from two replicas is two events and both survive.

**A filesystem "copy" pretending to be an object store.** The export config accepted `s3://` and `gs://` URLs, but the code turned the destination into a `PathBuf` and called `std::fs::copy`. An S3 target silently wrote to a local directory named `s3:`. Ketchup already had the right tool in the tree — `object_store`, which the snapshot uploader uses — so export now parses the destination through `object_store::parse_url` and `put`s each file. A bare path still works (we normalise it to `file://`), so nothing in the local tests changed; `s3://` and `gs://` now mean what they say.

**A checkpoint that skipped reused filenames — and a second one behind its back.** Export is incremental: a checkpoint records which files have shipped so a restart doesn't re-send everything. The old checkpoint keyed on filename. But log files are named `logs_NNNNNN.parquet` from a counter that resumes past the highest file on disk, so once retention prunes every file the counter resets to zero and a later flush reuses `logs_000000.parquet` for *different* bytes — which the filename-keyed checkpoint skips forever. We key the checkpoint on a durable id instead: the filename plus a hash of its contents, so a reused name with new bytes is a new object. And `relish logs-export` used to keep its own competing checkpoint in the same directory; both now share one Bun-owned file, so a manual export and the agent's export loop can't skip or double-ship each other's work.

**A doc comment describing a feature that didn't exist.** `relish logs-export`'s doc said it "triggers an immediate export from the running Bun agent's LogStore" and "falls back to direct file copy if the agent is unreachable." Neither sentence was true. The function never contacted the agent — there was no endpoint to contact — and it read two hardcoded local paths in the *opposite* preference order from the one bun itself uses, so an operator with a custom `[storage] logs` path got "no log store found" from a machine with gigabytes of logs on it. When a doc comment and its function disagree, you have two choices: fix the doc, or make the doc true. We made it true, because the described design was simply better: a `POST /v1/logs/export` endpoint on the agent runs the export server-side against the store's real directory with the Bun-owned checkpoint, the destination (`s3://`, `gs://`, or a path) resolves with the agent's credentials, and the CLI calls it when the agent answers a health probe — falling back to the direct local read, now in the agent's own path order, only when nothing is listening. The endpoint is Admin-only: it writes files wherever you point it, with the agent's credentials, which is not a thing a Deployer token should be able to do.

One more, quieter fix rode along: a clean shutdown used to drop whatever the flush loops had buffered since their last tick. The stop path now forces a final flush of both the metrics and log buffers after the workers have joined, so the last minute survives a restart. And we deleted the dead `KetchupStore` — a second, older log store that Bun constructed and never used, whose calendar/index code and `logs.max_file_size_mb` setting drove nothing. `LogStore` is the live path; the dead one is gone.

## What we learned

### Reuse the query engine, don't build one

DataFusion gives us SQL parsing, query planning, columnar execution, predicate pushdown, and Parquet I/O. That's roughly 200,000 lines of code we didn't write. Our glue layer is about 400 lines. The ratio (500:1) is the best leverage in the entire project.

The temptation was to build something simpler: a custom iterator over Parquet files with hardcoded filters. It would have been "enough" for v1. But then you want time-range queries, then aggregations, then LIKE filters, then JSON field extraction, and suddenly you've built half a query engine badly. Start with DataFusion and you skip the reinvention.

### Five default alerts cover 90% of incidents

We thought operators would want to define custom alert rules from day one. In practice, the five defaults (CPU throttle, OOM risk, memory high, disk high, CPU idle) catch nearly every production incident that metrics can detect. Custom rules are a Phase 11 feature, and nobody has complained about the delay.

The lesson: don't build config for things that have obvious defaults. Ship the defaults, add config later if someone needs it.

### "How far back do we look?" is not "how stale may this be?"

Node A reports 95% CPU. A second later, node B reports 10%. If we keep only
one reading per metric name, B's healthy reading hides A's problem. The regression
`healthy_series_cannot_hide_another_nodes_alert` reproduces exactly that failure.

The evaluator now keeps each `MetricKey`: the metric name plus its sorted label
map. Its state belongs to `(rule_name, labels)`. Two nodes, or two namespaces
running an app with the same name, have independent pending timers, firing
states and recoveries. `BTreeMap` gives the labels a stable order; deriving `Ord`
and `PartialOrd` on our private `AlertInstance` lets Rust compare those compound
keys without hand-written comparison code. We reuse the existing metric type
rather than inventing another representation at the query boundary.

Freshness remains a separate decision. `QUERY_WINDOW_SECS` bounds the query;
`MAX_VALUE_AGE_SECS` decides whether a returned reading is usable. Memory and disk
percentages require fresh numerator and denominator readings with identical
labels. Invalid labels and non-finite values provide no recovery evidence.
Missing data cancels an inconclusive pending timer but leaves a firing alert
active. Only a healthy reading from that same series resolves it.

Labels travel with API statuses, dashboard rows and webhook notifications.
PagerDuty's deduplication key includes a SHA-256 digest of the canonical label
JSON, so resolving A cannot close B's incident. Diagnostic collection also keeps
the labels when deduplicating repeated reports of the same incident. App-scoped
diagnostic collection remains explicitly unsupported; not every metric carries
an application identity.

The tests exercise independent pending, firing and recovery transitions, missing
and stale data, namespace collisions and notification identities. One writes two
labelled series to a real Parquet-backed store and queries them through the same
path the production evaluator uses. Keeping labels in a unit-test map wouldn't
help if the SQL path discarded them first.

### Server-rendered HTML with meta refresh beats React

The Brioche dashboard is a single server-rendered HTML page. No JavaScript framework, no API calls, no state management. The browser refreshes every 5 seconds. Total payload: 10KB. Time to first meaningful paint: zero seconds (it's all in the HTML response).

Could we build a nicer dashboard with React and WebSocket updates? Sure. But that's a separate build pipeline, a node_modules tree, a bundler, and an entire frontend ecosystem to maintain. The server-rendered approach gives us something that works today and costs nothing to maintain.

## Tests

Almost everything in this chapter is a pure data transform: a sample becomes a `RecordBatch`, a SQL string becomes rows, a threshold-and-duration becomes an alert state. Pure transforms are the easy case for testing — no I/O, no async, no cluster. So Phase 6 leans almost entirely on unit tests, and there's a lot of them.

### Unit tests — the bulk of the work

The three subsystems carry their own tests at the bottom of each source file:

- **Mayo (metrics):** Arrow schema validation, DataFusion SQL over the metrics table, Parquet round-trips, Prometheus text parsing, and the alert state machine. The alert tests read like the transition table itself — `inactive_to_pending_on_breach`, `pending_to_firing_after_duration`, `firing_to_inactive_on_recovery`, `pending_to_inactive_on_recovery`, `missing_metric_does_not_fire`. Each builds an evaluator, feeds it a metric value, and asserts the resulting state. The hardening work added a matching set of failure-path tests, one per edge from the previous section: `query_metric_name_injection_is_neutralised` and `app_metrics_name_injection_cannot_bypass_predicate` (the SQL escape), `resent_window_does_not_double_count` and `restart_resumes_flush_counter_without_clobbering` (idempotent, durable rollups), `stale_telemetry_does_not_resolve_a_firing_alert` (the value-not-boolean state machine), `slack_payload_matches_provider_shape` and `pagerduty_payload_matches_events_v2_shape` (the provider webhook contracts), and `query_proceeds_during_flush` plus `corrupt_parquet_file_does_not_fail_query` (the off-lock flush and corrupt-file skip). Each names the failure it prevents.
- **Ketchup (logs):** `append_and_query`, grep/tail/time-range filters, and the SQL path (`app` filter, time range, `LIKE` grep, `LIMIT`). The log-path hardening added a matching set of failure tests, one per edge above: `bounded_sql_rejects_non_select`, `bounded_sql_rejects_other_tables` and `bounded_sql_caps_returned_rows` (the seatbelt on `/v1/logs/sql`); `unreachable_node_is_a_partial_failure` and `grep_value_with_ampersand_and_question_mark_transmitted_intact` (honest, correctly-encoded fan-out); `identical_lines_from_two_replicas_both_survive` and `separated_duplicates_from_one_node_dedup` (the stable dedup identity); `reused_filename_with_new_contents_is_not_skipped` (durable checkpoint ids); and `flush_shared_persists_the_buffer_on_shutdown` (the final flush on stop).
- **Brioche (dashboard):** HTML rendering, and two security-flavoured tests worth calling out — `render_app_detail_escapes_html` (no stored-XSS through an app name) and `render_app_detail_masks_encrypted_env` (a secret never reaches the page). These are unit tests because the renderer is a pure function from data to a string; you assert on the string.

### End-to-end: the demo script

Unit tests prove each transform. To watch the whole pipeline breathe — collect, store, query, render — there's a script:

```sh
make observability-demo
```

It builds and starts `bun`, waits about twenty seconds (two ten-second collection cycles) so real CPU and memory samples accumulate, then queries the metric names, the summary, and the alert list over the HTTP API, and finally prints the dashboard URL. It's the fastest way to confirm the chapter's code actually works on your machine, not just in the test harness.

### Running them

Everything here runs under a plain `cargo test`. No gated tests in this chapter — no root, no eBPF, no network, no platform-specific runtime. To run a single subsystem:

```sh
cargo test --lib mayo        # metrics
cargo test --lib ketchup     # logs
cargo test --lib brioche     # dashboard
make observability-demo      # live, end-to-end
```

The cross-node and aggregation pieces — querying logs across the whole cluster, hierarchical metric rollups, exporting to S3 — are *advanced* observability, and their integration tests (`tests/metrics_aggregation.rs`, `tests/logs_cross_node.rs`, `tests/log_export.rs`) belong to Chapter 11. This chapter is the single-node foundation they build on.

All of these run in the portable suite: `make test` (which drives them through nextest). No root, no eBPF, no network, no platform-specific runtime, and no fixed sleeps — the flush concurrency test drives both the write and the read to completion with `tokio::join!` rather than guessing at a delay. Chapter 15 covers the suite taxonomy and why a test that can pass without executing its promised behaviour is worse than no test.

### Report export failures before the disk fills

The disk-pressure loop already refused to delete content that hadn't been
exported, but it discarded export errors. A broken destination could therefore
leave the disk filling with no explanation. `PressureResult` now carries an
optional export error, and Bun prints it with the affected store's name.

The regression writes a log file, configures an unsupported export destination,
and sets the pressure threshold below the file's size. It asserts both that the
failure is reported and that the local file survives. Reporting a failed backup
mustn't turn it into permission to delete the only copy.

### Offline exports must report partial success

Copying the archive is only half an incremental export. We also need to persist
which files were copied. The offline CLI used to discard a checkpoint write
failure and print success. It now returns an error that says the files arrived
but a later export may repeat them. The `?` operator propagates that error before
we print the success message. A CLI regression makes the checkpoint path a
directory, then checks both the copied bytes and the non-zero exit status.

`relish logs-export --source PATH --dest PATH` explicitly selects a local store,
including a custom store whose agent is stopped. A second regression exports
twice and verifies that the saved checkpoint suppresses the second copy.
Destinations must be UTF-8 because the object-store interface takes text;
rejecting an invalid path is safer than silently exporting to a different one.

### Preserve each archive generation

A local flush counter can restart at zero after retention removes every file.
That makes `logs_000000.parquet` a reusable filename, not a permanent identity.
Previously the checkpoint noticed new bytes, but the remote write still replaced
the old object. We now put the full SHA-256 digest in both the checkpoint identity
and the archive filename. The `.parquet` extension remains at the end so existing
SQL archive queries discover both generations.

The regression uses actual Parquet files and DataFusion. It exports one batch,
saves and reloads the checkpoint, removes the local file, restarts the store and
exports a second batch under the same local name. Querying the archive must return
both rows. Checking only the number of successful uploads missed the original bug.

Old short-hash checkpoints cannot establish that an immutable object exists.
Surviving source files are therefore exported again under the new names. Existing
legacy archive objects are left untouched; a mixed legacy/new archive can contain
duplicate rows for that migration batch. We prefer that explicit migration
limitation to deleting an old object whose provenance we cannot establish.

### An acknowledgement belongs to one destination

Exporting to archive A doesn't mean the same bytes exist in archive B. The
checkpoint now includes a scope derived from the destination URL and node prefix.
Changing either clears its acknowledgements. We store a hash of the scope rather
than the URL itself, so a checkpoint doesn't copy credentials embedded in a URL.
This hash identifies the export context; it is not an authentication mechanism.

An old checkpoint without a scope is also untrusted for skipping uploads. The
immutable names from the previous fix make repeated exports safe. Keeping one
active scope is deliberately simple: switching back to an earlier destination
may repeat writes, but it cannot mistake another archive's receipt for this one.

The pruner checks the scope as well as the content identity. Our regression
exports to a working destination, switches to an unwritable destination, then
forces disk pressure. The source must survive. Another test changes both the
destination and node prefix across checkpoint reloads and queries every archive.

### Own the whole export transaction

Sharing a checkpoint pathname didn't serialise its writers. The periodic task,
disk-pressure task, API handler and offline CLI could each load an old snapshot
and later replace a newer one. They now call the same transaction: acquire a
non-blocking file lock, load the latest checkpoint, export, then persist it before
returning success. A competing writer gets a busy error and can retry. Corrupt or
unreadable state is an error, not an empty receipt.

The lock is a separate persistent file. Replacing the JSON atomically must not
replace the inode that other processes lock. Checkpoint writes use a private
unique temporary file, sync its contents, rename it, and sync the parent directory.
Disk-pressure cleanup stops if export or checkpoint persistence fails. The agent
reports the error through its existing export-error path instead of discarding it.

The blocking persistence closure takes ownership of the lock with `move`. A
closure is Rust's anonymous function; `move` transfers captured values into it.
Here that matters if the async caller is cancelled: the blocking filesystem write
can finish while still holding its lock. Dropping the caller must not admit the
next writer before the old rename finishes. On return, the committed snapshot
replaces the caller's borrowed checkpoint; stale caller state never drives uploads.

Regressions exercise a held lock, stale snapshots, corrupt checkpoint data,
agent exports and the offline CLI's non-zero error result. Actual power-loss and
storage-device durability qualification remains part of the release recovery gate.

### Report source failures

A directory whose name ends in `.parquet` used to look like an empty successful
export: the read failed and the loop continued. We now reject non-regular entries
and invalid filenames, and propagate directory and file I/O errors with their
source path. The only skipped read error is `NotFound`, because retention can
remove an immutable file between enumeration and opening it.

Directory enumeration and file reads use Tokio's asynchronous filesystem API.
The directory regression runs on both supported host platforms. The invalid-byte
filename regression is Linux-only: macOS APFS rejects that filename when creating
the fixture, before our exporter can inspect it. That platform boundary belongs
in the test definition rather than a silent successful early return.

### Retention counts successful removals

Metrics and rollup pruning previously ignored `remove_file` errors and incremented
the deletion count anyway. A directory named `blocked.parquet` was enough to make
both stores claim they had reclaimed a file that still existed. They now increment
only after a successful removal and return filesystem failures with the affected
path. A concurrent `NotFound` is harmless but does not count as our deletion;
a not-yet-created store directory remains an empty store. Directory enumeration
errors also reach the caller. A failed pass can have removed earlier files, so
callers must treat its error as incomplete retention, not an all-or-nothing rollback.
Both store regressions verify that the failed candidate's contents survive.

### Keep receipts for live source generations

An export checkpoint is evidence that a current source file reached its configured
destination. It need not be a permanent catalogue of the archive. After a complete
successful scan, we retain only IDs read from current source files. The same locked,
durable transaction commits this compacted set. Failed scans leave the previous
checkpoint intact.

This bounds receipt count by live source generations rather than export history;
unlimited source retention still means unlimited live receipts. Archived objects
remain immutable and untouched. If an old source reappears, writing its content
hash key again is safe. A 32-generation retention/restart regression keeps one
receipt throughout, skips an immediate duplicate export, and queries all 32
archived generations afterwards. Destination and pruning tests still apply to
the smaller checkpoint.

Local rollup receipts prevent repeated ingestion by one aggregator. Cluster query
merging also retains the original worker/minute/series key, so reassignment to
another aggregator cannot double-count overlapping history. Chapter 11 explains
the owned-row endpoint and its persistence and HTTP regressions.


### A worker does not need a listening socket

During Linux upgrade qualification, one node could not bind its API. A reporting
worker on another node had taken that port with an ephemeral listener. Why was
it listening? The same transport type had been used for both ends of reporting,
even though workers only send snapshots and rollups.

`TcpReportingSender` implements the existing `ReportingTransport` trait with the
same framing, TLS connector and node fault gate. Its receive method returns
`None` immediately. It owns no listener or accept task. The aggregator keeps the
full transport because it actually receives reports. This also removes two
unused sockets and tasks from every node.

The transport regression checks fault-gated delivery and the absence of an
inbound stream. The integration test sends reports from two outbound-only
workers to a real TCP aggregator. Upgrade tests also bound HTTP requests, so a
socket that accepts connections without answering cannot hide the failure
behind an unbounded read. These checks fix the observed listener collision;
they do not establish that every upgrade failure has the same cause.


### Qualifying the parser behind an archive

A dependency advisory named Thrift, which our Parquet reader uses for metadata.
Updating that crate is only part of the repair. Parquet also has a private
compact-protocol decoder. Its integer loop kept shifting until an input byte
said to stop, and used a wrapping shift. Sixty-four continuation bytes could
therefore turn malformed metadata back into a plausible value.

The regression writes a real one-row Parquet file, changes the metadata integer,
updates the footer length and asks the public reader to open it. The untouched
file must work; the malformed one must return an error. This checks the parsing
path we actually ship, rather than merely comparing dependency version numbers.

We keep the query engine on DataFusion 45 and use Thrift 0.23 with a small,
reviewable patch to Parquet 54.3.1. The upstream source, licence, archive checksum
and exact patch live under `vendor/parquet`. Cargo's `[patch.crates-io]` section
makes every dependent crate use that same parser. It is a temporary maintained
dependency, with removal criteria recorded beside it.

The decoder now visits only the bit positions that fit the wire integer. Each
step also checks that the final byte's payload fits the remaining bits. Lengths
and 32-bit integers get a 32-bit budget; 64-bit values get 64 bits. Unknown-field
skipping uses these same methods, so adding an unfamiliar field cannot bypass
the check. We also reject list counts larger than the remaining metadata before
allocation, and replace a truncated-double indexing panic with an EOF error.

The tests include a valid maximum-width integer to make sure refusal hasn't
become indiscriminate. Normal metric, rollup and log persistence/query tests
remain part of qualification. A dependency scan is useful evidence. A parser
regression is different evidence, and we need both.

### A log query owns its response bodies

A peer can send `200 OK` and then stop sending the JSON body. Timing only
`request.send()` doesn't protect us: that future completes when the headers
arrive. The query can still wait indefinitely while reading the entries. We
wrap the whole request and body-read future in the node's existing timeout.
Expiry contributes a named node failure; responsive nodes' entries still count.

Cancellation has another ownership trap. Dropping a Tokio `JoinHandle` detaches
its task, so dropping a vector of handles doesn't stop the network requests.
We use `JoinSet` instead. This collection owns its spawned tasks and aborts them
when the set is dropped. `join_next().await` yields one completed task at a time:
`None` means the set is empty, while `Some` contains either the task's result or
a task failure. We retain both sorts of failure in the fan-out result.

Two socket fixtures reproduce the old mistakes. One sends headers and the
first byte of a body, then stalls. The other waits until those headers have
been sent before cancelling the parent query and checks that the peer sees its
connection close. The fixture accepts either EOF or a TCP reset after
cancellation: macOS can reset a connection when its client discards unread
response bytes. Both prove closure. Other I/O errors still fail, and the
original two-second observation deadline remains. A caller's timeout is useful
only if the work it owns ends too.

### An empty alert list needs evidence

Suppose a node returns `200 OK` with `{}`. Does that mean there are no alerts?
The old client used a missing-field fallback and answered yes. The diagnostic
collector could then present a healthy result without having received an alert
inventory at all.

Bun and Relish now share `AlertsResponse`, whose `alerts` field contains
`Vec<AlertStatus>`. Serde must find that list and decode each required field.
`AlertPhase` is an enum with `Inactive`, `Pending` and `Firing` variants;
`#[serde(rename_all = "lowercase")]` keeps their existing JSON spellings.
An unknown phase fails decoding instead of disappearing from the report.
The evaluator's separate `AlertState` still owns its evaluation timestamps;
the API carries the small serialisable snapshot consumers need.

The client, TUI and `wtf` collector carry these typed statuses all the way
through. A malformed inventory stays a collection error. An explicit
`{"alerts": []}` is valid evidence of an empty inventory. Labels remain
optional for older responses, and pending/firing timestamps remain optional;
neither changes whether the required rule, phase, severity and description
can be decoded. HTTP fixtures exercise both refusals and a labelled firing
response, preserving the existing wire values without contacting a cluster.
