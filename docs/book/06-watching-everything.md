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

One refresh call cost us a release candidate. `System::refresh_all()` looks like the obvious choice, but in sysinfo 0.33 it refreshes processes *without* removing the ones that have exited, and sysinfo keeps each tracked process's `/proc/<pid>/stat` file open so the next refresh is cheaper. Bun raises its open-file limit to about a million, so nothing capped that cache either. The V02 soak's workloads start a short-lived process every second or so, and after half an hour one node's Bun held 1,532 open files (992 of them `stat` files for processes long gone) and 951 MB of memory. The collector now refreshes memory, CPU and processes separately, asks for dead processes to be removed, and turns sysinfo's file cache off. `exited_processes_are_forgotten_on_refresh` starts twenty `sleep`s, lets one refresh see them, kills them, and checks the next refresh has forgotten every one; with `refresh_all` it still tracks all twenty.

Collection runs every 10 seconds. Each sample is a `(timestamp, metric_name, labels_json, value)` tuple, stored as an Arrow RecordBatch. When the batch fills up, it's flushed to a Parquet file.

## Prometheus scraping

Not everything comes from system stats. Your apps expose their own numbers (requests served, queue depth, how long a checkout takes) on a `/metrics` endpoint in the Prometheus text format. For a long time Reliaburger could only scrape a fixed list of URLs from the node config, which is fine for a node exporter and useless for an app with three replicas that move between nodes. Who writes those URLs? And who rewrites them after a deploy?

So an app now says where its metrics live, and the cluster works out the rest:

```toml
[app.web]
port = 8080
metrics = {}                     # scrape http://<instance>:8080/metrics
# metrics = { port = 9797, path = "/prom" }
```

`port` defaults to the app's port and `path` to `/metrics`. A Kubernetes manifest gets the same thing from the `prometheus.io/scrape`, `prometheus.io/port` and `prometheus.io/path` annotations on its pod template, which is how half the charts on Artifact Hub already say "scrape me". The podinfo demo declares `prometheus.io/port: "9797"`, so `relish apply -f podinfo.yaml` gives you `metrics = { port = 9797 }`, and the importer stops listing 9797 among the ports it drops.

### Every node scrapes its own

Prometheus runs one server that discovers every target and pulls from all of them. We already have a process on every node that knows exactly which instances it's running and at what address: Bun. So each Bun scrapes its *own* instances and nobody else's.

That choice pays for itself three times. There's no discovery to get wrong, because the agent that started the container is the one asking. The scrape never crosses the network: a runc container is reached at its container IP, the same address health checks use, and a process workload on loopback. The metrics port doesn't need publishing or routing, because the node is already inside the right network. And the samples land in the local Mayo store, next to that instance's CPU and memory, so the query fan-out we built for per-app metrics finds them with no changes.

The scrape loop runs beside the collector in `src/bin/bun.rs`. Every `app_scrape_interval_secs` (10 by default, the same as the collector, so an app's own series and its CPU line up) it asks the agent for targets over the command channel and does the HTTP work itself:

```rust
let (response, targets) = tokio::sync::oneshot::channel();
if scrape_cmd_tx
    .send(AgentCommand::ScrapeTargets { response })
    .await
    .is_err()
{
    break;
}
let Ok(targets) = targets.await else { continue };
scrape_app_targets(&scrape_mayo, &client, &targets, &scrape_node, timeout).await;
```

The agent answers from state it already holds (the deployed specs and each instance's container IP) and goes straight back to its loop. A hung app can stall the scrape task, but never the agent.

### A bounded fan-out with streams

A node might run forty instances. Scraping them one after another means one slow app delays the other thirty-nine; spawning forty tasks means no limit at all. The `futures` crate has exactly the tool:

```rust
let results: Vec<(AppScrapeTarget, Result<Vec<CollectedMetric>, ScrapeError>)> =
    futures_util::stream::iter(targets.to_vec())
        .map(|target| {
            let client = client.clone();
            async move {
                let result = fetch_metrics(&client, &target.url, timeout).await;
                (target, result)
            }
        })
        .buffer_unordered(MAX_CONCURRENT_SCRAPES)
        .collect()
        .await;
```

A *stream* is the async cousin of an iterator: it yields values over time, and you drive it with `.await` instead of a `for` loop. `stream::iter` turns our list into one. `.map` turns each target into a *future*, the not-yet-run work of scraping it (an `async move { ... }` block is an anonymous function body that runs later and takes ownership of what it uses). `buffer_unordered(16)` is the interesting part: it keeps at most sixteen of those futures running at once and hands results back in whatever order they finish. If you know Go, it's a worker pool with a semaphore, in one line. `.collect()` gathers everything into a `Vec`.

The first version borrowed `target` and `client` inside the future instead of owning them. It compiled as a plain function and failed the moment the loop ran inside `tokio::spawn`, with the wonderfully opaque "implementation of `FnOnce` is not general enough". The borrowed version ties each future's lifetime to the function's borrows, and the compiler can't prove that combination is `Send` (safe to move between threads, which the multi-threaded runtime requires). Giving every future its own copy fixed it. A `reqwest::Client` is an `Arc` inside, so cloning it costs a reference count, not a connection pool.

Each fetch is also bounded three ways: a timeout (half the interval, at most five seconds), an 8 MiB body cap, and a 20,000-sample cap. The endpoint belongs to the app, and a node shouldn't buffer whatever an app feels like sending.

### Labels are the whole point

A sample with no labels is a number without a story. Every scraped sample gets four:

| Label | Example | Why |
|---|---|---|
| `app` | `default/web` | What every per-app query already filters on, and what process metrics use |
| `namespace` | `default` | Tenancy |
| `instance` | `default__web-0` | One line per replica on a chart |
| `node` | `node-02` | Where to go looking |

What if the app already sets `instance` itself? Prometheus renames the app's label to `exported_instance`, and so do we. Silently overwriting it would throw the app's data away; keeping it would mean two labels fighting over one name.

All samples from one sweep also share one timestamp. That sounds pedantic until you add series up: `http_requests_total{status="200"}` plus `{status="500"}` is the instance's total only if both were recorded at the same instant. The old code stamped each sample as it was inserted, so a scrape that straddled a second boundary split in two.

Finally, every target gets an `up` sample: 1 when the scrape worked, 0 when it didn't. A dead metrics endpoint then shows up as data you can query, not as a quiet gap.

### Histograms, stored properly

The previous parser had a bug hiding in one line:

```rust
prometheus_parse::Value::Histogram(buckets) => {
    buckets.iter().map(|b| b.count).sum::<f64>()
}
```

A Prometheus histogram's buckets are *cumulative*: `le="0.1"` counts requests up to 100 ms, `le="0.5"` counts those plus everything up to 500 ms, and so on. Summing them counts fast requests several times over and produces a number that means nothing. The fix stores what Prometheus stores, one series per bucket plus `_sum` and `_count`:

```rust
prometheus_parse::Value::Histogram(buckets) => {
    for bucket in buckets {
        let mut labels = labels.clone();
        labels.insert("le".to_string(), format_bound(bucket.less_than));
        push_finite(&mut metrics, format!("{}_bucket", sample.metric), labels, bucket.count);
    }
}
```

With `_sum` and `_count` side by side, mean latency over an interval is simply the increase in `_sum` divided by the increase in `_count`.

### Rates, and the counter that went down

Counters only go up. So when one goes *down*, the process restarted and started again from zero. `mayo::series::rates` turns a counter's points into per-second rates and treats a drop as a reset, the way Prometheus's `rate()` does:

```rust
pub fn rates(points: &[Point]) -> Vec<Point> {
    points
        .windows(2)
        .filter_map(|pair| {
            let [(before_at, before), (at, value)] = [pair[0], pair[1]];
            let elapsed = at.checked_sub(before_at).filter(|elapsed| *elapsed > 0)?;
            let increase = if value >= before { value - before } else { value };
            Some((at, increase / elapsed as f64))
        })
        .collect()
}
```

`windows(2)` walks a slice two elements at a time, overlapping: `[a, b]`, `[b, c]`, `[c, d]`. It hands you a borrowed sub-slice, not a copy, so there's no allocation. The next line *destructures* both pairs at once: `let [(before_at, before), (at, value)] = ...` pulls four named values out of an array of two tuples, the way Python's `(a, b), (c, d) = ...` does. The `?` after `filter(...)` works inside the closure because the closure returns an `Option`: no elapsed time means `None`, and `filter_map` drops that step.

This module is shared. `relish metrics` and the dashboard both start from the same raw rows and need the same arithmetic, so it lives in one place, away from HTTP and rendering, with unit tests for resets, gaps and division by zero.

### Reading it back

`relish metrics web` lists what was scraped, one number per metric, added up across instances:

```text
METRIC                         TYPE       SERIES  INSTANCES  VALUE
http_request_duration_seconds  histogram       2          2  mean 11.9ms
http_requests_total            counter         4          2  8.40/s
up                             gauge           2          2  2
```

How does it know a counter from a gauge without a `TYPE` line? By name, the way Prometheus's own conventions intend: `_total` is a counter, and `_sum`/`_count` are when they come as a pair. `--name` shows one metric per instance with a rate and a sparkline, and a histogram named by its base (`--name http_request_duration_seconds`) shows mean latency per instance. It asks for only the newest two samples of each series (`per_series=2`), which is plenty for a rate and cheap across a big cluster.

That parameter fixed a real bug on the way. The per-app query sorted oldest first and stopped at 10,000 rows, so on a busy app the newest samples, the ones anything called "latest" needs, were the first to be cut. The query now sorts newest first before the limit and flips the rows back, and the endpoint defaults to the last fifteen minutes instead of the beginning of time.

The dashboard's app charts had a bug of their own: `brioche.js` expected an array, the per-app endpoint answered `{data, warnings}`, and the script returned early. Every app chart was empty, always. They now read from a small chart endpoint that returns series already lined up per instance, `{timestamps, series: [{label, values}]}`, with counters as rates and histograms as means, so the browser only draws. A Rust test pins that shape, because that's the contract the JavaScript relies on. An app with scraped metrics also gets a requests-per-second chart (its `http_requests_total`, or failing that its own first counter) and a latency chart.

Configure fixed URLs with `[[metrics.scrape_targets]]` when there's no app to hang them on (a node exporter, say); those keep their `job` as the `app` label.

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

Ketchup stores logs in the same Arrow/DataFusion/Parquet stack that Mayo uses for metrics. The schema started as five columns: `timestamp`, `app`, `namespace`, `stream`, and `line`. (It has seven now; "Tails that tell the truth" below explains the other two.) Want to find all errors from the web app in the last hour?

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

**Per-app metrics that were never collected.** Production only collected node-level metrics (CPU, memory for the whole box). The autoscaler and the per-app dashboards had nothing to read. The collector already knew how to scrape a single process; it just wasn't being called. The collection loop now asks the agent for its running instances and collects per-process CPU and memory for each one, labelled `namespace/app`. (Even then the autoscaler kept asking for a series called `cpu` that nobody records. Chapter 9 tells that story.)

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

## Following the whole cluster

`relish logs web` has asked every node for years. `relish logs web -f` didn't:
it followed the node you happened to be talking to. On a laptop cluster that's
node 1, and the scheduler had just put two of your three replicas on nodes 2
and 3. You'd watch one replica and wonder why the others were so quiet.

We had two places to fix it. The CLI could open one stream per node, or the
node could do it and hand the CLI one merged stream. The laptop settled it:
behind Lima's user-mode network, the host can reach node 1's forwarded port and
nothing else. So the node does the fan-out. When a follow arrives without
`local=true`, the node starts a background task that reads the app's
placements from the council every two seconds, opens a stream to each placed
node (with `local=true&label=true`, so the peer doesn't fan out again and
stamps each line `[node instance]`), and pushes everything into one channel
that feeds the client's server-sent-event response.

The task keeps a map from node name to the `AbortHandle` of that node's
stream. `tokio::spawn` gives you a `JoinHandle`; calling `.abort_handle()` on
it gives you something smaller that can cancel the task but can't wait for
it, which is all a supervisor needs. The loop itself waits on three things at
once:

```rust
tokio::select! {
    () = events.closed() => break,
    Some(ended) = ended_rx.recv() => { /* forget it, warn if it failed */ }
    () = tokio::time::sleep(LOG_FOLLOW_REFRESH) => {}
}
```

`select!` polls every branch and runs whichever finishes first, dropping the
others. Go programmers will recognise `select` on channels; the difference is
that Rust's version works on any future, including a timer and "the client
went away" (`Sender::closed`), not just channel operations. When the client
disconnects, the first branch wins and the whole fan-out winds down.

A node that goes away gets exactly one `warning` event, whichever way it
went: a broken connection, a clean end during a graceful shutdown, or a
stream still hanging on a dead TCP connection that only the membership table
knows is gone. The CLI prints warnings to stderr and lines to stdout, so
`relish logs web -f | grep ERROR` still works while a node dies.

Both ends of that pipe parse SSE, so the parsing lives in one small type,
`ketchup::sse::SseDecoder`. It's incremental on purpose: a network read can
end anywhere, including halfway through a multi-byte UTF-8 character, so the
decoder buffers bytes and only decodes a block once it has seen the blank line
that ends it.

`relish top` had the same blind spot and a subtler trap. The dashboard already
charted `process_cpu_percent` and `process_memory_bytes`, labelled by app and
PID. Why not have the CLI query those metrics and match them to instances by
PID? Because a PID only means something on the node that issued it, and three
VMs booted from the same image hand out very similar PIDs. So each node joins
its own statuses to its own samples (`bun::top::node_rows`), and the node you
ask merges finished rows. A PID never crosses a machine boundary without the
node it belongs to.

## Tails that tell the truth

The V02 soak runs a tiny app that appends 1, 2, 3, … to a file ten times a
second and logs `ACK n` after each append. Every few minutes the harness runs
`relish logs soak-writer --tail 20` and saves the answer. The file on disk was
always gapless. The tail wasn't:

```
ACK 967
ACK 968
ACK 974
ACK 969
ACK 975
ACK 970
```

After a node's Bun was SIGKILLed, or its VM powered off, it got stranger:
lines from an hour earlier showed up as the newest. And a CI run showed the
Redis client counting up in twos. Three symptoms, three causes, and none of
them in the app.

**One-second timestamps with no tie-breaker.** `LogStore` stamped each line
with `as_secs()` and queries said `ORDER BY timestamp`. At ten lines a second,
ten rows share every timestamp. SQL doesn't promise any order among equal
keys, and DataFusion reads the Parquet files and the in-memory buffer as
separate partitions and merges them, so rows from two flushes that landed in
the same second came back interleaved. That's the `968, 974, 969, 975`
pattern, and a unit test that writes 200 lines inside one second, flushing
every nine, reproduced it exactly.

**`LIMIT` without `DESC`.** The same query appended `LIMIT n` for a tail. `ORDER
BY timestamp LIMIT 20` is the *first* twenty rows. A cluster query hid this by
fetching everything and trimming after the merge, but a single node answered
`--tail 20` with the oldest twenty lines it had.

**Re-reading capture files from the start.** When Bun restarts, it adopts
running containers and starts a log forwarder for each. The runtime's
`follow_logs` began at byte zero, so the store received the container's whole
history again and stamped every line with *now*. Hour-old `ACK`s became the
newest rows. The Redis client's "stepping by two" was the same thing: an old
instance from a rolling deploy, which had shared the counter with its
replacement and so only ever saw every other value, got re-ingested wholesale
and filled the tail.

The fix gives every row an identity that means something.

A new `sequence` column holds the ingest time in nanoseconds, bumped so it
rises strictly on each node:

```rust
let sequence = nanos.max(self.ingested.last_sequence.saturating_add(1));
self.ingested.last_sequence = sequence;
```

`saturating_add` is Rust's way of saying "add, but stick at `u64::MAX`
instead of wrapping". Plain `+` on integers panics on overflow in a debug
build and wraps silently in release, so for a value that must only ever grow
we pick the behaviour explicitly. Queries now `ORDER BY sequence`, and a tail
is a subquery that takes the newest N and puts them back in order:

```sql
SELECT * FROM (... ORDER BY sequence DESC LIMIT 20) AS tailed
ORDER BY sequence
```

Why nanoseconds and not a plain counter? A counter orders one node's rows
perfectly and says nothing about two nodes. Nanoseconds order each node
exactly and interleave nodes as well as their clocks agree, which is the most
anyone can promise without a shared clock. The cross-node merge sorts on
`(sequence, node)`, so an exact tie still renders the same way every time,
and it deduplicates on `(node, sequence)`. The earlier dedup key,
`(node, timestamp, stream, line)`, had its own quiet bug: an app that printed
the same line twice in one second lost one of them.

The forwarder problem needed the runtime's help. A capture file is
append-only, so the byte offset just past a line's newline names that line
for good. A new `grill::capture::CaptureReader` turns raw chunks into lines
and remembers that offset for each one, counting raw bytes so a line of
invalid UTF-8 (which becomes `U+FFFD` in the `String`) doesn't shift the
positions after it. `follow_logs` now sends a `CapturedLine` carrying the
stream and that position, which also means stderr is finally labelled as
stderr instead of everything being called stdout.

The runc runtime reads stdout and stderr through one reader each:

```rust
let mut readers = [
    (LogStream::Stdout, "stdout"),
    (LogStream::Stderr, "stderr"),
]
.map(|(stream, extension)| {
    CaptureReader::new(stream, Some(stem.with_extension(extension)))
});
```

That's `map` on a fixed-size array, `[T; N]`, not on an iterator. It returns
another array of the same length, `[CaptureReader; 2]`, with no `Vec` and no
heap allocation. Go has fixed arrays too but no way to map over one; in
Python you'd get a list back.

The store then does the bookkeeping. It keeps the highest offset it has
ingested per capture file and refuses anything at or below it:

```rust
if let Some(position) = &record.position {
    let seen = self.ingested.offsets.get(&position.file).copied();
    if seen.is_some_and(|offset| position.end_offset <= offset) {
        return false;
    }
    self.ingested.offsets.insert(position.file.clone(), position.end_offset);
}
```

`Option::is_some_and` is "there's a value and this predicate holds for it",
which reads better than `matches!(seen, Some(offset) if ...)`. On every flush
the store writes those offsets, plus the last sequence, to
`ingest-checkpoint.json`, always *after* the Parquet file. Which
order you pick decides what a crash between the two writes costs. Checkpoint
first, and a crash loses the batch: the offsets say the lines are stored, and
they aren't. Parquet first, and a crash stores one batch twice. We take the
duplicate. The same reasoning covers a power cut: lines still in the buffer
never reached a checkpoint, so the replay after the reboot stores them, once,
and skips everything older.

Keying on the file path works because each runc generation writes its own
capture files, and the process runtime only ever appends. A restarted
instance is a new file and starts from its first line; an adopted one resumes
where the store left off. The Apple runtime is the exception: `container
logs --follow` hands us lines with no offsets, so an adopted Apple container
is still ingested again after a restart. It's a laptop runtime and the
comment in `apple.rs` says so.

Last, the old-and-new-instance overlap is real during a rolling deploy, so
hiding it would be wrong. Each row now records the `instance` that wrote it,
and when a tail spans more than one instance `relish logs` prefixes each line
with `[instance]`, the way `relish logs -f` already did. Two clients
incrementing one counter now look like two clients.

What about the Parquet files a node wrote before `sequence` existed? Our
first cut made the column nullable so they'd still read, with `NULLS FIRST`
to sort them before everything new. Then we deleted it. Before 0.1.0 we
don't carry old formats forward; we bump the generation and start a fresh
cluster (Chapter 14 has the policy). The logs table is durable state and
the `/v1/logs/entries` answer is a node-to-node wire format, so both moved:
protocol 24 to 25, state 40 to 41. A node upgraded in place now refuses to
start at its state stamp instead of quietly half-reading old log files, and
`sequence` is a required column. Only `instance` stays nullable, because
the node's own startup lines don't come from any instance.

The checkpoint itself goes through the same `atomic_write` helper the
identity code uses: a uniquely named temp file, `fsync`, rename over the old
checkpoint, `fsync` the directory. `flush_replaces_the_checkpoint_atomically`
checks that two flushes leave exactly one complete checkpoint and no temp
files behind.

### Ask everyone, not just the current home

All of that shipped, and the next soak still caught a stale tail. Right
after a graceful stop and start of the whole cluster, the Redis client's
tail read `INCR 749 … 782`, counting in twos, while the counter was past
2,500. Forty seconds later the tail was right again.

We went looking for another re-ingestion bug and built tests for every way
one could happen: a graceful restart with a retired instance's capture file
still on disk, a graceful stop with lines only in the buffer. Both passed.
The store was fine. The status snapshots told the real story. The client had
started on node 1, moved to node 3 during a test run, then to node 2, where
it spent half an hour. After the restart the scheduler put it back on node 1.

And the cluster-wide query only asked the nodes where the app was placed
*now*. Node 1's newest stored lines for the app were from the moment it left,
thirty minutes earlier, during a rolling overlap (hence the twos). Node 2,
which held everything since, was never asked. Nothing was out of order. We
were just asking the wrong nodes.

Lines live where they were produced, and they stay there when the app moves.
Placement records where an app runs, not where it ran. So the fan-out now
asks every live member, and a node that never ran the app answers with an
empty list:

```rust
pub fn query_targets(placed: &[String], members: &[(String, String)]) -> QueryTargets {
    QueryTargets {
        reachable: members.to_vec(),
        unreachable: placed
            .iter()
            .filter(|node| !members.iter().any(|(id, _)| id == *node))
            .cloned()
            .collect(),
    }
}
```

`members.to_vec()` copies the borrowed slice into an owned `Vec`, which needs
the element type to be `Clone`; `(String, String)` is, because both halves
are. Placement still matters for one thing: a placed node gossip no longer
lists is certainly holding lines we want, so it comes back as a warning
rather than silently shrinking the answer.

Is asking every node expensive? Each node answers with only its own tail and
the query is interactive, so on a laptop's three nodes it costs nothing. At
the ten thousand nodes we design for it's a real cost, and one we'll watch.
The alternative, recording
every node an app ever ran on, is state the cluster would have to keep
forever for the sake of a log query. `tail_after_an_app_moves_back_includes_the_nodes_it_ran_on_meanwhile`
reproduces the soak with two stores and fails against the old node choice.

## When nothing looks like success

Both hardening passes share a pattern, and later reviews kept finding more of it: a failure that comes back dressed as an empty, successful answer. A directory called `blocked.parquet` made the exporter and both retention loops report success. A peer that sent `200 OK` and then went quiet hung a log query. A node that answered `{}` convinced the diagnostic collector there were no alerts. None of these crash. They lie quietly, which is worse.

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

### One exporter at a time

Keying the checkpoint on content wasn't the whole story: the *archive* still used the plain filename, so a reused `logs_000000.parquet` overwrote its predecessor in S3. The object key now carries the SHA-256 too (`{node}/{sha256}-logs_000000.parquet`), so an archived object never changes. The checkpoint also records a hash of the destination and node it covers, because a receipt from bucket A says nothing about bucket B. Change either and the old receipts are cleared; the worst case is a repeated upload to an immutable key.

The last hole was concurrency. The periodic export, the disk-pressure loop, the API handler and an offline `relish logs-export` could each load the checkpoint, export, and save a stale copy over a newer one. Now they all go through `export_logs`, which takes a non-blocking lock on a separate `_export_checkpoint.lock` file (separate, because the checkpoint itself is replaced by an atomic rename), reloads the latest checkpoint, uploads, and persists before returning. Its tail:

```rust
let committed = tokio::task::spawn_blocking(move || {
    let _lock = lock;
    current.save(&path).map_err(|error| {
        KetchupError::Io(std::io::Error::other(format!(
            "files exported but checkpoint could not be saved: {error}; retry is safe"
        )))
    })?;
    Ok::<_, KetchupError>(current)
})
.await
.map_err(|error| KetchupError::Io(std::io::Error::other(error.to_string())))??;
*checkpoint = committed;
```

`move || { ... }` is a closure (an anonymous function) that takes ownership of what it captures: `lock`, `current` and `path`. If the async caller is cancelled, the blocking thread keeps running and still *owns* the lock, so no second exporter gets in before the rename finishes. `let _lock = lock;` keeps the lock alive until the closure ends; a name starting with `_` just silences the unused-variable warning. Writing `let _ = lock;` would be a bug, because a bare `_` binds nothing and the lock would drop (and unlock) on the spot.

`Ok::<_, KetchupError>(current)` uses the "turbofish" `::<>` to name the closure's error type, which the compiler can't infer by itself; `_` lets it fill in the rest. The `??` unwraps two layers: `spawn_blocking` returns an error if the thread panicked, and inside that sits our own `Result`. Finally, `*checkpoint = committed` writes through the caller's `&mut` reference, so the caller only ever sees a committed snapshot.

#### Busy is not broken

The V02 soak turned that lock into a wall of red. Every node's journal said `log export failed during disk pressure: io error: export checkpoint is busy` a few hundred times, with `log export error: ... busy` close behind. Was the soak losing logs?

No. `check_and_relieve` returns early whenever its export fails, and it only prunes a file whose exact bytes are in the checkpoint, so a busy lock left every file on disk. The cause was mundane: Bun starts the 60-second export timer and the 300-second disk-pressure timer together, and 300 is a multiple of 60, so every fifth export tick lands on a disk-pressure tick. One of the two loses the `try_lock`. When the periodic export lost, nothing happened: the other exporter was shipping the same files. When disk pressure lost, it skipped pruning for five minutes, which with an 8 MB cap is exactly when you want it to prune.

So a busy lock is now its own error variant, `KetchupError::ExportBusy`, instead of a stringly-typed `io::Error`. `std::fs::File::try_lock` already tells the two cases apart:

```rust
match lock.try_lock() {
    Ok(()) => {}
    Err(std::fs::TryLockError::WouldBlock) => return Err(KetchupError::ExportBusy),
    Err(std::fs::TryLockError::Error(error)) => return Err(KetchupError::Io(error)),
}
```

The periodic task matches `Err(KetchupError::ExportBusy) => {}` and stays quiet. Disk pressure waits for its turn instead:

```rust
loop {
    match export_logs(source_dir, destination, node_id, checkpoint).await {
        Err(KetchupError::ExportBusy) if tokio::time::Instant::now() < deadline => {
            tokio::time::sleep(EXPORT_BUSY_POLL).await;
        }
        other => return other,
    }
}
```

The `if` after the pattern is a *match guard*: the arm only matches when the pattern fits and the condition holds, so a busy error past the 60-second deadline falls through to `other` and is reported like any failure. Why poll every 100 ms rather than call the blocking `lock()` in `spawn_blocking`? Because a thread parked in `flock` can't be cancelled. If the caller gave up, the thread would still wake up later holding the lock with nobody to release it until it finished. A `tokio::time::sleep` is dropped cleanly on shutdown. The test holds the lock from outside, releases it after 300 ms, and checks that the same `check_and_relieve` call then exports and prunes the file.

### Errors that used to vanish

Retention in the metrics and rollup stores ignored `remove_file` errors and counted the file as deleted anyway; now only successful removals count. The exporter forgives exactly one read failure, `NotFound`, because retention can delete a file between listing the directory and opening it.

Log fan-out had two ownership bugs. Timing `request.send()` isn't enough, because that future finishes when the headers arrive and a peer can then stall mid-body, so the timeout now covers the body read too. And dropping a Tokio `JoinHandle` *detaches* its task rather than cancelling it, so an abandoned query left its requests running. `fan_out_query` now spawns into a `tokio::task::JoinSet`, which owns its tasks and aborts them all when it's dropped.

The alert inventory used to decode `{}` as "no alerts". Bun and Relish now share one response type with a required list:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertsResponse {
    /// Observed statuses; a missing field is not evidence of an empty list.
    pub alerts: Vec<AlertStatus>,
}
```

Serde refuses to decode a struct when a field without `#[serde(default)]` is missing, so only an explicit `{"alerts": []}` now means "nothing is firing".

## What we learned

### Reuse the query engine, don't build one

DataFusion gives us SQL parsing, query planning, columnar execution, predicate pushdown, and Parquet I/O. That's roughly 200,000 lines of code we didn't write. Our glue layer is about 400 lines. The ratio (500:1) is the best leverage in the entire project.

The temptation was to build something simpler: a custom iterator over Parquet files with hardcoded filters. It would have been "enough" for v1. But then you want time-range queries, then aggregations, then LIKE filters, then JSON field extraction, and suddenly you've built half a query engine badly. Start with DataFusion and you skip the reinvention.

### Five default alerts cover 90% of incidents

We thought operators would want to define custom alert rules from day one. In practice, the five defaults (CPU throttle, OOM risk, memory high, disk high, CPU idle) catch nearly every production incident that metrics can detect. Custom rules are a Phase 11 feature, and nobody has complained about the delay.

The lesson: don't build config for things that have obvious defaults. Ship the defaults, add config later if someone needs it.

### "How far back do we look?" is not "how stale may this be?"

The first evaluator queried the last 120 seconds and kept the newest row per metric *name*, discarding the timestamp and the labels. Dropping the timestamp made the query window double as a freshness guarantee, so a metric that stopped arriving 110 seconds ago still looked live. Those are different questions, and they now have different names: `QUERY_WINDOW_SECS` and `MAX_VALUE_AGE_SECS`.

Dropping the labels was worse. Node A reports 95% CPU; a second later node B reports 10%. Keep one reading per name and B's healthy number hides A's problem. So an alert is keyed on the rule *and* the series:

```rust
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AlertInstance {
    rule_name: String,
    labels: BTreeMap<String, String>,
}
```

The evaluator keeps a `BTreeMap<AlertInstance, AlertState>`. A `BTreeMap` is sorted, so its keys must be orderable, and deriving `PartialOrd` and `Ord` gives us that: Rust compares fields in declaration order, with no hand-written comparison. Each series now has its own pending timer, firing state and recovery, and only a fresh healthy reading from the *same* series resolves an alert. The labels reach the webhooks too, so resolving A can't close B's PagerDuty incident.

### A test that passed for the wrong reason

A dependency advisory flagged the Thrift crate that Parquet used to decode file metadata: a crafted length could make it reserve absurd amounts of memory. Parquet 59 replaced Thrift with its own decoder, and DataFusion 55 uses Parquet 59, so we upgraded DataFusion by ten major versions. It changed two lines of ours, because we never name `parquet` in `Cargo.toml`; we use DataFusion's re-export (`datafusion::parquet::...`) and always get the version DataFusion was built against. We kept regression tests that corrupt a real Parquet file's metadata and check that the reader refuses it.

We ran them on a Mac, they passed, and we moved on. Then Linux CI aborted the entire test process: "memory allocation of 206158430112 bytes failed". The impossible-count test declares two billion schema elements, and Parquet 59 calls `Vec::with_capacity` with that count before checking there are bytes to back it. macOS happily hands out 206 GB of address space you never touch, and the decoder fails on the next byte with an ordinary error. Linux refuses the reservation, and Rust's default response to a failed allocation is to abort the process. The same corrupted archive that returned `Err` on a laptop would take down a production node.

Upstream had fixed it in Parquet 60, but DataFusion 55 still needs 59. So we forked arrow-rs, applied that one commit to the 59.3.0 tag, and pointed Cargo at the fork:

```toml
[patch.crates-io]
parquet = { git = "https://github.com/reliaburger/arrow-rs", rev = "34ac1864f214da1648b05fbd1a2b6de4f2b4a952" }
```

`[patch.crates-io]` replaces a crates.io package everywhere in the build, so DataFusion picks up the fixed decoder too. We patch all fifteen arrow-rs crates to the same revision: patching only `parquet` would build a second copy of the Arrow crates, and Rust treats a type from one copy as unrelated to the "same" type from the other. The block goes once DataFusion moves to a fixed Parquet. The lesson: a test only proves something on the platform where it ran.

### Server-rendered HTML with meta refresh beats React

The Brioche dashboard is a single server-rendered HTML page. No JavaScript framework, no API calls, no state management. The browser refreshes every 5 seconds. Total payload: 10KB. Time to first meaningful paint: zero seconds (it's all in the HTML response).

Could we build a nicer dashboard with React and WebSocket updates? Sure. But that's a separate build pipeline, a node_modules tree, a bundler, and an entire frontend ecosystem to maintain. The server-rendered approach gives us something that works today and costs nothing to maintain.

## Tests

Almost everything in this chapter is a pure data transform: a sample becomes a `RecordBatch`, a SQL string becomes rows, a threshold-and-duration becomes an alert state. Pure transforms are the easy case for testing — no I/O, no async, no cluster. So Phase 6 leans almost entirely on unit tests, and there's a lot of them.

### Unit tests — the bulk of the work

The three subsystems carry their own tests at the bottom of each source file:

- **Mayo (metrics):** Arrow schema validation, DataFusion SQL over the metrics table, Parquet round-trips, Prometheus text parsing, and the alert state machine. The alert tests read like the transition table itself — `inactive_to_pending_on_breach`, `pending_to_firing_after_duration`, `firing_to_inactive_on_recovery`, `pending_to_inactive_on_recovery`, `missing_metric_does_not_fire`. Each builds an evaluator, feeds it a metric value, and asserts the resulting state. The hardening work added a matching set of failure-path tests, one per edge from the previous section: `query_metric_name_injection_is_neutralised` and `app_metrics_name_injection_cannot_bypass_predicate` (the SQL escape), `resent_window_does_not_double_count` and `restart_resumes_flush_counter_without_clobbering` (idempotent, durable rollups), `stale_telemetry_does_not_resolve_a_firing_alert` (the value-not-boolean state machine), `slack_payload_matches_provider_shape` and `pagerduty_payload_matches_events_v2_shape` (the provider webhook contracts), and `query_proceeds_during_flush` plus `corrupt_parquet_file_does_not_fail_query` (the off-lock flush and corrupt-file skip). Each names the failure it prevents.
- **Ketchup (logs):** `append_and_query`, grep/tail/time-range filters, and the SQL path (`app` filter, time range, `LIKE` grep, `LIMIT`). The log-path hardening added a matching set of failure tests, one per edge above: `bounded_sql_rejects_non_select`, `bounded_sql_rejects_other_tables` and `bounded_sql_caps_returned_rows` (the seatbelt on `/v1/logs/sql`); `unreachable_node_is_a_partial_failure` and `grep_value_with_ampersand_and_question_mark_transmitted_intact` (honest, correctly-encoded fan-out); `identical_lines_from_two_replicas_both_survive` and `repeated_identical_lines_from_one_node_both_survive` (the `(node, sequence)` dedup identity); `tail_returns_the_newest_lines_in_emission_order`, `lines_within_one_second_keep_emission_order_across_flushes` and `lines_sharing_a_second_come_back_in_sequence_order` (the V02 ordering bugs); `restart_does_not_reingest_lines_already_flushed`, `lines_lost_with_the_buffer_are_ingested_again_after_a_crash` and `refollowing_a_capture_file_replays_the_same_positions_and_the_store_keeps_one_copy` (exactly-once ingestion across restarts); `logs_from_several_instances_name_each_line_s_instance` (labelled tails); `reused_filename_with_new_contents_is_not_skipped` (durable checkpoint ids); and `flush_shared_persists_the_buffer_on_shutdown` (the final flush on stop).
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

The cross-node and aggregation pieces — querying logs across the whole cluster, hierarchical metric rollups, exporting to S3 — are *advanced* observability, and their integration tests (`tests/suite/metrics_aggregation.rs`, `tests/suite/logs_cross_node.rs`, `tests/suite/log_export.rs`) belong to Chapter 11. This chapter is the single-node foundation they build on.

All of these run in the portable suite: `make test` (which drives them through nextest). No root, no eBPF, no network, no platform-specific runtime, and no fixed sleeps — the flush concurrency test drives both the write and the read to completion with `tokio::join!` rather than guessing at a delay. Chapter 15 covers the suite taxonomy and why a test that can pass without executing its promised behaviour is worse than no test.
