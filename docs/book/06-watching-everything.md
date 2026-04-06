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

## The dashboard

Brioche is a single HTML page. No React, no Vue, no webpack. The server renders the HTML with current data, embeds a 2KB CSS stylesheet, and sends it. The browser refreshes every 5 seconds via a `<meta http-equiv="refresh">` tag.

The dashboard shows three sections: apps (name, status, instance count), nodes (name, state, app count), and alerts. Status dots are green for healthy, amber for pending, red for failed. The dark theme is easy on the eyes during those late-night debugging sessions.

Total payload: under 10KB. First paint: instant.

## Test count

Phase 6 adds 96 tests, bringing the total to 967. The new tests cover Arrow schema validation, DataFusion SQL queries, Parquet persistence, system metrics collection (CPU, memory, disk, network), Prometheus text parsing, alert state machine transitions, log append/query/grep/tail/JSON filtering, sparse index binary search, and dashboard HTML rendering.
