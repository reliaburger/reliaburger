# Ketchup: Log Collection System

**Component:** Ketchup (Log Collector)
**Status:** Design (this document reconciled against the shipped code)
**Whitepaper Reference:** Section 15

---

## 1. Overview

Ketchup is Reliaburger's built-in log collection subsystem. It captures the output of every managed container and process workload on a node, stores it as flat Parquet files, and provides querying, export, and retention management without any external dependencies.

Ketchup runs as a module within the Bun agent on every node. There is no separate log collection daemon, no sidecar container, and no central log aggregation server. Each node captures and serves the logs produced by workloads running on it. Non-follow cross-node queries are coordinated by the leader using the same fan-out pattern Mayo uses for metrics -- dispatched to the nodes running the target app, then merge-sorted -- **but log *follow* (`-f`) is local-node only** (see §3.7). Transport is HTTP throughout; there is no gRPC.

The storage engine is **Parquet + DataFusion SQL**, mirroring `MayoStore` exactly. This document has been reconciled against the code: the per-app binary log-record format, the sparse memory-mapped `.idx` index, per-day file rotation, the `.log`/`.log.zst` compression lifecycle, regex grep, RFC3339 time ranges, the `--instance` and dot-path `--json-field` filters, and the log-permission model are all **planned -- not yet implemented** and flagged as such below.

The design goals for Ketchup are:

- **Zero configuration by default.** Log capture is automatic for every workload.
- **Low per-app overhead.** Lines are forwarded over a channel into an in-memory buffer and flushed to Parquet; the archive read path uses Parquet row-group statistics and bloom filters (on `app`/`namespace`) to skip irrelevant groups.
- **SQL query support.** Logs are queryable with DataFusion SQL over the `logs` table. Substring search (`--grep`) is a SQL `LIKE '%…%'` match. Structured JSON field querying (`--json-field`) is a **planned** convenience layered on top (today it is a client-side single-key match, §5.3).
- **Built-in retention and export.** Logs are retained for `retention_days` (default 7) and pruned oldest-first. Export ships the ZSTD-compressed Parquet files as-is (not `jsonl.gz`) to an `object_store` destination (local path, `file://`, `s3://`, `gs://`).

---

## 2. Dependencies

### Internal Dependencies

| Component | Dependency Type | Purpose |
|-----------|----------------|---------|
| **Bun** (Agent) | Host process | Ketchup runs as a module inside Bun. Bun provides the lifecycle hooks for container start/stop events and access to container stdio file descriptors. |
| **Grill** (Container Runtime Interface) | Stdio streams | Grill manages the container runtime (containerd/runc). When a container is created, Grill returns the stdout and stderr file descriptors (or named pipes) that Ketchup attaches to for log capture. For process workloads, Bun spawns the process with redirected stdout/stderr pipes that Ketchup reads from. |
| **Mustard** (Gossip) | Cluster topology | Ketchup uses the Mustard membership list to know which nodes are running which apps, enabling the leader to fan out cross-node log queries to the correct subset of nodes. |
| **Meat** (Scheduler) | App placement | The leader uses Meat's placement data to determine which nodes to query when a `relish logs` command targets an app with multiple instances across the cluster. |

### External Dependencies

None. Ketchup has no external dependencies for core functionality. Export destinations (S3, GCS, HTTP) are optional and configured by the operator.

---

## 3. Architecture

### 3.1 Capture Pipeline

```
Container/Process workload
    │
    └─ stdout ──→ runtime line stream ──→ agent forwarder task
                                               │  (builds a LogRecord;
                                               │   stream is ALWAYS Stdout)
                                               ▼
                                          mpsc channel
                                               ▼
                                       LogStore.append()  ──flush──▶
                                       logs_NNNNNN.parquet
```

Bun's agent (`src/bun/agent.rs`) spawns a forwarder task per instance that receives complete lines from the runtime, wraps each in a `LogRecord { app, namespace, stream, line }`, and sends it over an `mpsc` channel to the shared `LogStore`. `LogStore::append` stamps the wall-clock second and pushes the line into an in-memory buffer; a periodic flush turns the buffer into a Parquet file (§3.2).

**stderr is never distinguished from stdout.** The forwarder always sets `stream: LogStream::Stdout` -- there is no separate stderr capture path, and the Apple Container runtime nulls stderr entirely. Every stored line therefore carries `stream = "stdout"`. The `LogStream::Stderr` variant exists in the type and round-trips through the schema, but nothing writes it. **A true separate-stderr capture story is planned -- not yet implemented.**

The capture task is spawned when Bun starts a workload and cancelled when it stops. Backpressure flows through the bounded channel and the runtime's own pipe buffering.

### 3.2 On-Disk File Layout

**Status of the per-app/per-day `.log`/`.idx`/`.log.zst` layout below: planned -- not yet implemented.** There is no per-app directory, no per-day file, no separate index file, and no `.log`/`.log.zst` lifecycle. What ships is a single flat directory of Parquet files, mirroring `MayoStore`:

```
<storage.logs>/                    # default /var/lib/reliaburger/logs
├── logs_000000.parquet
├── logs_000001.parquet
├── logs_000002.parquet
└── ...
```

Each file is one flush of the in-memory buffer. File names come from a counter seeded one past the highest existing `logs_NNNNNN.parquet`, so a restart resumes numbering instead of clobbering. Files are written with ZSTD column compression and small (8192-row) row groups, plus bloom filters on the `app` and `namespace` columns so archive reads can skip row groups that hold no matching rows.

Every file carries the schema:

| Column | Arrow type | Meaning |
|--------|-----------|---------|
| `timestamp` | `UInt64` | seconds since Unix epoch |
| `app` | `Utf8` | app name |
| `namespace` | `Utf8` | namespace |
| `stream` | `Utf8` | always `"stdout"` today (see §3.1) |
| `line` | `Utf8` | the captured line |

**Day-boundary rotation at midnight UTC: planned -- not yet implemented.** Files roll over on flush, not on a calendar boundary; a line lands in whichever flush is current.

### 3.3 Log File Format

**Status: planned -- not yet implemented.** There is no length-prefixed binary record format, no `record_length`/`stream`/`instance_id`/`flags` framing. Lines are stored as Parquet rows (§3.2). Efficient seeking comes from Parquet row-group statistics and predicate pushdown, not a bespoke binary layout.

### 3.4 Timestamp Index

**Status: planned -- not yet implemented.** There is no `.idx` file, no fixed-size `IndexEntry`, no memory-mapped sparse index, and no `INDEX_INTERVAL`. Time-range queries are `WHERE timestamp BETWEEN …` predicates DataFusion pushes down to Parquet row groups.

### 3.5 JSON Auto-Detection

**Status: planned -- not yet implemented.** Ketchup does not sample the first N lines, does not parse them with `serde_json`, and stores no `is_json` flag. `--json-field` is a client-side filter applied to whatever lines come back (§5.3); the store treats every line as opaque text.

### 3.6 Compression Pipeline

**Status of the hourly `.log`→`.log.zst` seekable-frame pipeline: planned -- not yet implemented.** Compression is not a separate lifecycle stage. Parquet files are written ZSTD-compressed at flush time, per row group, so they are already both compressed and randomly accessible. There is no `compression_level`/`compression_frame_size` knob and no `.tmp`→rename dance.

### 3.7 Cross-Node Query Architecture

```
relish logs web --since 1h --grep "ERROR"          (one-shot, no -f)
    │
    ▼
/v1/logs/query/{app}/{namespace}  on the receiving node
    │
    ├─ Lookup nodes running "web" (council placement state)
    ├─ Fan out GET /v1/logs/entries/{app}/{namespace} to each (HTTP, not gRPC)
    ├─ Each node runs LogStore::query locally (DataFusion SQL over Parquet)
    └─ Merge-sort the returned LogEntry lists by timestamp
```

Non-follow queries fan out over HTTP (`fan_out_query` in `src/ketchup/query.rs`) to the nodes running the app and merge-sort the results by timestamp.

**Follow (`-f`) is local-node only.** `relish logs web -f` opens `GET /v1/logs/{app}/{namespace}?follow=true`, which streams (SSE, or WebSocket via `ws_logs_handler`) new lines from **that node's** agent only, via the `FollowLogs` agent command. There is no cross-node merge of live streams -- the "leader opens a stream to all nodes and merge-sorts" behaviour is **planned -- not yet implemented**. Cross-node aggregation exists only for the non-follow query path.

---

## 4. Data Structures

### 4.1 Core Types

These are the shipped types (`src/ketchup/types.rs`). Note what is **absent** versus earlier drafts: a `LogEntry` has no `node`, `instance`, or `is_json` field, and `LogStream` is a plain enum, not a `#[repr(u8)]` value used in a binary record.

```rust
/// Which output stream a line came from. In practice only Stdout is ever
/// written (see §3.1); Stderr exists but is never produced.
pub enum LogStream { Stdout, Stderr }

/// A captured line, tagged with its source, before it enters the store.
pub struct LogRecord {
    pub app: String,
    pub namespace: String,
    pub stream: LogStream,   // always Stdout in practice
    pub line: String,
}

/// A single log entry as returned by a query.
pub struct LogEntry {
    pub timestamp: u64,      // seconds since Unix epoch (not nanos)
    pub stream: LogStream,
    pub line: String,
}
```

There is no `IndexEntry`, no `LogIndex`, and no `memmap2` -- the sparse memory-mapped index is **planned -- not yet implemented** (§3.4).

### 4.2 Query Types

```rust
/// Parameters for a log query (src/ketchup/types.rs).
pub struct LogQuery {
    pub app: String,
    pub namespace: String,
    pub start: Option<u64>,             // inclusive, seconds since epoch
    pub end: Option<u64>,               // inclusive, seconds since epoch
    pub grep: Option<String>,           // SUBSTRING match, not regex
    pub json_field: Option<(String, String)>,  // single (key, value)
    pub tail: Option<usize>,            // last N lines
}
```

The differences from earlier drafts are load-bearing:

- **`grep` is a plain substring**, applied as SQL `line LIKE '%…%'`. There is no `GrepFilter`, no compiled `regex::Regex`, and no case-insensitive `--grep-i` (the `regex` crate is not a dependency here). **Regex grep: planned -- not yet implemented.**
- **`json_field` is a single `(key, value)` pair**, matched **client-side** against the top-level key only. Multiple ANDed filters and dot-path traversal (`request.method`) are **planned -- not yet implemented**.
- There is **no `instance` filter**, **no `node` filter**, **no `stream` filter**, and **no `until`/`end`-as-follow** field. `--instance` does not exist. **`--until`: planned -- not yet implemented** (the CLI has `--since` only).
- Follow is not a field on `LogQuery`; it is a separate agent command (§3.7).

### 4.3 Export and Retention Configuration

The shipped log config is small (`LogsSection` in `src/config/node.rs`): `retention_days` (default 7), `export_path` (optional), `export_interval_secs` (default 3600), `max_storage_mb` (default 0 = unlimited). See §6.

**Status of the richer config below: planned -- not yet implemented.** There is no `LogExportConfig` struct, no `ExportFormat` (the export format is Parquet-as-is, **not** `jsonl.gz`), no `apps`/`fields` include/exclude selection, no `compressed_retention_days`, and no `ByteSize` string parser (`max_storage_mb` is a plain integer of megabytes). Export ships the ZSTD-Parquet files unchanged so an exported archive is queryable with the same DataFusion path as local logs.

### 4.4 Internal Capture State

**Status: planned -- not yet implemented.** There is no `CaptureHandle` with separate `stdout_task`/`stderr_task`/`*_json_detected` fields, and no `DayLogWriter` (no per-day file, no `.idx`, no `instance_id` mapping, no `chrono::NaiveDate`). The shipped capture is a per-instance forwarder task feeding a shared `LogStore` over an `mpsc` channel (§3.1); the store buffers in memory and flushes to Parquet (§3.2).

---

## 5. Operations

### 5.1 Log Capture

Bun's agent (`src/bun/agent.rs`) receives complete lines from the runtime's stdout stream, builds a `LogRecord { app, namespace, stream: Stdout, line }`, and forwards it over an `mpsc` channel to the shared `LogStore`. Container and process workloads use the same path.

As noted in §3.1, only stdout is captured: the forwarder always tags `LogStream::Stdout`, and the Apple Container runtime nulls stderr. **Separate stderr capture is planned -- not yet implemented.**

The `AsyncFd`/two-tasks-per-stream/`DayLogWriter` mechanics from earlier drafts, and the pipe-buffer reconnection guarantees around a Bun restart, describe the planned design, not the shipped forwarder.

### 5.2 Structured JSON Detection

**Status: planned -- not yet implemented.** There is no first-20-lines sampling, no `is_json` flag, and no per-stream detection buffer. The store keeps every line as opaque text; `--json-field` is a client-side match applied at query time (§5.3).

### 5.3 Querying

Query operations are initiated via `relish logs` (CLI), the Brioche UI, or the HTTP API. The CLI flags that exist are `--tail`, `-f/--follow`, `--grep`, `--since`, and `--json-field` (`src/relish/commands.rs`).

**One-shot fetch (default mode):**
```bash
relish logs web
```
Without `-f`, this is a **one-shot fetch**, not a tail/follow. It returns the (optionally `--tail`-limited) current logs and exits. It first tries the cross-node query endpoint (`/v1/logs/query/...`), which fans out over HTTP to the nodes running the app and merge-sorts by timestamp (§3.7).

**Follow:**
```bash
relish logs web -f
```
`-f` streams new lines from the **local node only** (§3.7). It is not a cluster-wide merged tail.

**Time range:**
```bash
relish logs web --since 1h
relish logs web --since 1739196000        # raw epoch seconds
```
`--since` accepts **raw epoch seconds or a duration** (e.g. `1h`, `30m`); it is parsed by `parse_since` into a start timestamp. **RFC3339 timestamps and `--until` are not supported** -- an `--until` flag does not exist. (RFC3339 parsing and an end bound: planned -- not yet implemented.)

**Text search (grep):**
```bash
relish logs web --grep ERROR
```
`--grep` is a **substring** match. Server-side it becomes `line LIKE '%ERROR%'`; the client also re-checks the substring. It is **not** a regex, and there is **no `--grep-i`** case-insensitive variant.

**Structured JSON field query:**
```bash
relish logs web --json-field level=error
```
A single `key=value` filter, applied **client-side**: each returned line is parsed as JSON and the **top-level** `key` compared as a string. Multiple `--json-field` flags and dot-path traversal (`request.method=POST`) are **planned -- not yet implemented**.

**Instance filter:** **Status: planned -- not yet implemented.** There is no `--instance` flag; logs are not tagged with an instance id.

### 5.4 Log Export

Export runs as a periodic background task on each node, configured via:

```toml
[logs.export]
destination = "s3://my-bucket/logs/"
format = "jsonl.gz"
interval = "1h"
```

**Export pipeline (as implemented):**

Logs are already stored as ZSTD-compressed Parquet (`logs_NNNNNN.parquet`). Export ships those files as-is rather than re-serialising to JSON lines, so an exported archive is queryable with the same DataFusion path as local logs.

1. Every `interval`, the export task lists the local `.parquet` log files.
2. For each file not yet recorded in the checkpoint, it reads the bytes and `put`s them to `{destination}/{node}/{filename}` through the `object_store` crate — one interface over a bare path, `file://`, `s3://` and `gs://`. Cloud credentials come from each backend's standard environment variables.
3. On a successful upload, the file's **durable id** is recorded in the checkpoint.

**Export file naming at the destination:**
```
s3://my-bucket/logs/<node>/logs_NNNNNN.parquet
```

**Export checkpoint persistence:** The checkpoint is a small JSON file (`_export_checkpoint.json`) in the log data directory, holding a set of durable ids. A durable id is `{filename}@{sha256-prefix}` — the filename plus a hash of its contents — *not* the filename alone. Log-file names come from a counter that resumes past the highest file on disk, so retention pruning that empties the directory resets the counter and a later flush reuses `logs_000000.parquet` for different bytes; a filename-keyed checkpoint would skip that reused name forever. Hashing the contents makes the reused name a new object. Bun's export loop and disk-pressure task are the only writers, and `relish logs-export` shares the same checkpoint file, so there is exactly one authoritative record of what has shipped. The checkpoint survives Bun restarts.

### 5.5 Retention Management

Retention is the same Parquet-file model as Mayo (`src/bun/disk_pressure.rs`, `check_and_relieve`), keyed on `retention_days` and `max_storage_mb`:

1. If `export_path` is set, un-exported Parquet files are shipped to the destination first; only files whose exact bytes are recorded in the export checkpoint become eligible for deletion.
2. Files are pruned **oldest-first by mtime** when past `retention_days` or when total size exceeds `max_storage_mb` (0 = unlimited, so only the retention cutoff applies).

**Status of the compression lifecycle: planned -- not yet implemented.** There is no `.log`→`.log.zst` compression step (Parquet is already ZSTD-compressed at write time), no separate `compressed_retention_days`, no per-app byte counter, no `log.storage_exhausted` alert, and no `relish events --type log-retention` stream. Pruning deletes whole Parquet files.

### 5.6 Cross-Node Log Aggregation

For **non-follow** queries, `/v1/logs/query/{app}/{namespace}` looks up which nodes run the app (council placement state), fans the query out over **HTTP** to each node's `/v1/logs/entries/...`, and merge-sorts the returned `LogEntry` lists by `timestamp` (`fan_out_query` in `src/ketchup/query.rs`). Transport is HTTP; there is no gRPC and no newline-delimited streaming protocol.

**Follow across all nodes is planned -- not yet implemented.** `-f` streams from the local node only (§3.7). The k-way live merge with a 100ms clock-skew window belongs to the planned design.

---

## 6. Configuration

All log configuration lives under the `[logs]` section (`LogsSection` in `src/config/node.rs`). It has just four keys; unknown keys are rejected (`deny_unknown_fields`), so a config that still sets the planned keys below gets a clear error naming the field.

```toml
[logs]
# Days to retain log Parquet files before pruning. Default: 7.
retention_days = 7

# Optional export destination for Parquet log files. When set, un-exported
# files are periodically shipped here via object_store. Accepts a local path,
# a file:// URL, or an object-store URL (s3://…, gs://…). Default: unset.
# export_path = "s3://bucket/logs/"

# How often to export (seconds). Default: 3600 (1 hour).
export_interval_secs = 3600

# Maximum local log Parquet storage (MB). 0 = unlimited (the default).
# When exceeded, exported files are pruned oldest-first.
max_storage_mb = 0
```

**Status: planned -- not yet implemented (none of these keys exist).** `compressed_retention_days`, `index_interval`, `compression_level`, `compression_frame_size`, `maintenance_interval`, the `[logs.export]` sub-table with `format = "jsonl.gz"`/`apps`/`fields`, and per-app overrides (`[app.*]` `logs.retention_days`/`logs.max_line_length`/`logs.suppress_stderr`). `max_storage` is not a byte-size string; it is the integer `max_storage_mb`. Export format is Parquet, not `jsonl.gz`.

### Configuration Defaults and Rationale

| Parameter | Default | Rationale |
|-----------|---------|-----------|
| `retention_days` | 7 | Covers a typical on-call rotation; a week of logs enables post-incident review. |
| `export_interval_secs` | 3600 | Hourly export keeps the object-store archive current without excessive churn. |
| `max_storage_mb` | 0 (unlimited) | Off by default; set it to cap local log growth, at which point exported files are pruned oldest-first. |

---

## 7. Failure Modes

### 7.1 Storage Exhaustion

**Trigger:** local log Parquet exceeds `max_storage_mb`, or `retention_days` elapses.

**Response (`src/bun/disk_pressure.rs`):** if `export_path` is set, un-exported files are shipped first (and only checkpoint-recorded files become deletable); then files are pruned oldest-first by mtime while past retention or over the size cap. With `max_storage_mb = 0` only the retention cutoff prunes.

The emergency per-app line-dropping, the `log.storage_exhausted` alert, and the `relish events` drop counter from earlier drafts are **planned -- not yet implemented**. There is no `ENOSPC`-triggered eviction path; pruning runs on the periodic pressure check.

### 7.2 Container Restart During Capture

When a workload stops, its forwarder task ends; a new one is spawned when a new instance starts. **Partial-line flushing with an `is_truncated` marker is planned -- not yet implemented** -- there is no truncation flag on a stored line (the record has no `flags` field).

### 7.3 Corrupt Parquet File

**Trigger:** power loss or a crash mid-flush leaves a truncated or garbage `.parquet` file.

**Response:** on query the log store registers the Parquet directory as a DataFusion `ListingTable`; an unreadable file surfaces as a read/plan error for that scan. (Mayo's metrics store skips corrupt files per-file with a log; the corresponding per-file skip on the log read path is a reasonable follow-up.) There is no `.idx` file, so the "corrupt index / rebuild by scanning" story does not apply -- that whole mechanism is **planned -- not yet implemented** along with the index itself (§3.4).

### 7.4 Clock Skew

Timestamps are wall-clock seconds stamped at `append`. A backward clock jump can make stored timestamps non-monotonic; queries are ordinary `ORDER BY timestamp` SQL, which tolerates that. The monotonic-session-clock and 100ms cross-node merge window from earlier drafts are **planned -- not yet implemented**.

### 7.5 High-Volume Log Flood

Backpressure flows through the bounded `mpsc` channel from the forwarder into the store, and through the runtime's own buffering upstream of that. A `logs.drop_policy` knob is **planned -- not yet implemented** (see Open Questions).

---

## 8. Security Considerations

### 8.1 Log Access Control

**What ships:** the log routes require an authenticated token (`AnyToken` in `src/bun/authz.rs`). The per-app routes (`/v1/logs/{app}/{namespace}`, `/v1/logs/entries/...`, `/v1/logs/query/...`, and the follow/WebSocket variants) additionally enforce **tenant scope** via `authorize_scoped` before serving -- a scoped token can only read logs for its own namespace/app, and the scope check for a follow socket runs *before* the upgrade. The raw-SQL endpoint `/v1/logs/sql` reads across every tenant, so it deliberately **refuses scoped tokens** (it takes no app to scope by) and caps results (`MAX_LOG_SQL_ROWS`, a 256 MiB working-memory limit, read-only `SELECT`/`WITH` only).

**Status of the `logs:read`/`logs:export`/`logs:admin` permission model, the `viewer` role's line-limit, and per-permission scoping: planned -- not yet implemented.** There are no `logs:*` permissions; access is the `AnyToken` + tenant-scope model above.

Cross-node fan-out carries a service token between nodes; a per-request "original requester identity forwarded to the data source" model is **planned -- not yet implemented**.

### 8.2 Sensitive Data in Logs

Ketchup does **not** perform automatic redaction of sensitive data in log lines. This is a deliberate design choice: automatic redaction is unreliable (false positives corrupt debugging data, false negatives provide a false sense of security) and should be the responsibility of the application.

Both operator-facing data controls below are **planned -- not yet implemented**: there is no `[logs.export]` `fields.exclude` field filtering (export ships whole Parquet files unchanged), and there is no built-in log-scrubbing Job.

**Recommendation:** Applications should avoid logging secrets, tokens, passwords, and PII. Ketchup treats log lines as opaque data and stores them as-is.

### 8.3 Log File Permissions

Log Parquet files are written by the Bun process user under `storage.logs` and are not on any container's mount namespace, so a workload cannot read them directly -- access is via the query API (token + tenant scope, §8.1) or host-level filesystem access. **The explicit `0640` mode and configurable `log_group` are planned -- not yet implemented**; files get the process's default creation mode.

---

## 9. Performance

**Reconciliation note:** the figures below were derived for the planned binary-record + memory-mapped-index design. They do not describe the shipped store, which buffers `LogRecord`s over a bounded `mpsc` channel and flushes them to ZSTD-Parquet; there is no `DayLogWriter`, no `writev` of binary records, no 16-byte record overhead, no `.idx` binary search, no `mmap`, and no `regex`-crate grep (grep is Parquet `LIKE`). Read the subsections as targets for the planned design, not measurements of what ships.

### 9.1 Write Throughput

**Target:** Sustain capture from 500 concurrent apps per node with a combined log output of 100,000 lines/second (200 lines/second per app average) without dropping lines or introducing visible latency.

**Mechanism:** Each capture task writes to the `DayLogWriter` via a bounded `mpsc` channel (capacity: 8192 entries). The `DayLogWriter` batches pending entries and issues a single `writev()` syscall for multiple records, amortizing the syscall overhead. With an average log line of 200 bytes, the binary record overhead is 16 bytes (header), yielding ~216 bytes per record. At 100,000 lines/second, this is ~20.6 MB/s of sustained write throughput -- well within the capability of any modern SSD and even many spinning disks.

**Buffered I/O:** Log files are opened with `O_APPEND` and writes use a 64 KB userspace buffer (flushed on buffer full or every 100ms, whichever comes first). The 100ms flush interval bounds the maximum data loss on a process crash to 100ms of log output per app. The `fsync` is **not** called on every flush (this would destroy write throughput); instead, `fsync` is called once per minute. This means a kernel panic could lose up to 1 minute of log data, which is an acceptable tradeoff for the write throughput gain.

### 9.2 Query Latency

**Time-range query:** For a query over a 1-hour window in a 100 MB log file:

1. Index binary search: O(log(N/4096)) = O(log(~25000)) ~ 15 comparisons. This completes in < 1 microsecond.
2. Forward scan from the index entry: at most 4 KB of data to reach the start of the time window, then sequential scan through the matching records.
3. Total latency is dominated by disk I/O for the sequential scan, not by CPU. For data in the page cache (recent logs), expect < 10ms for a 1-hour window returning 10,000 matching lines.

**Grep query:** Regex matching via the `regex` crate operates at ~1 GB/s for simple patterns on modern CPUs (the crate uses SIMD-accelerated DFA). A grep over 10M lines (~2 GB of raw data) completes in ~2 seconds from cold cache, or < 500ms from warm cache.

**JSON field query:** Parsing each line with `serde_json` is the bottleneck for structured queries. `serde_json::from_str` parses at ~500 MB/s for typical JSON log lines. A JSON field query over 10M lines (~2 GB) takes ~4 seconds from cold cache. This is acceptable for ad-hoc debugging queries; for high-frequency structured queries, operators should export logs to a dedicated search engine (Elasticsearch, Loki).

### 9.3 Compression Ratio

Zstd at level 3 achieves typical compression ratios of 5:1 to 10:1 on log data, depending on the entropy of the log lines. Structured JSON logs compress better (8-10x) due to repeated key names. Free-form text logs compress less (4-6x). At 10x compression, 30 days of compressed logs for an app producing 1 GB/day of raw output requires ~3 GB of storage.

### 9.4 Memory Overhead

| Component | Memory | Notes |
|-----------|--------|-------|
| Per-capture-task buffers | ~8 KB | `BufReader` (4 KB) + line buffer (4 KB) |
| Per-app `mpsc` channel | ~1.7 MB | 8192 entries * ~216 bytes |
| Per-app `DayLogWriter` write buffer | 64 KB | Userspace write buffer |
| Per-open index (mmap) | ~2.4 MB | 10M lines / 64 * 16 bytes (virtual, not RSS) |
| Total per-app overhead | ~4.2 MB | Mostly virtual (mmap); RSS is ~2 MB |
| Total for 500 apps | ~2.1 GB virtual | RSS depends on access patterns; typically ~200-400 MB |

The dominant memory cost is the `mpsc` channel buffers. If memory pressure is a concern, the channel capacity can be reduced (at the cost of increased backpressure on high-volume apps).

---

## 10. Testing Strategy

**Reconciliation note:** the tables below list tests for the planned binary-format/index/JSON-detection/regex design and do not match the current suite. The shipped tests instead cover the Parquet `LogStore` (append/flush/reopen without clobbering, query by app + time range + substring grep + tail, distinct-app listing, SQL-injection escaping, bounded raw-SQL), the exported-Parquet query path (`remote_query`), and the `LogEntry`/`LogQueryResult` JSON round-trips. Read what follows as the target matrix for the planned features.

### 10.1 Unit Tests

| Test | Description |
|------|-------------|
| `test_log_file_roundtrip` | Write N `LogEntry` records to a `DayLogWriter`, then read them back and verify byte-for-byte equality. |
| `test_index_binary_search` | Create an index with known timestamps, then verify that `LogIndex::lookup()` returns the correct offset for various query timestamps (exact match, between entries, before first, after last). |
| `test_json_detection_all_json` | Feed 20 JSON lines to the detection buffer; verify `is_json = true`. |
| `test_json_detection_mixed` | Feed 19 JSON lines + 1 plain text line; verify `is_json = false`. |
| `test_json_detection_timeout` | Feed 5 JSON lines and wait 5 seconds; verify detection completes with `is_json = true`. |
| `test_grep_filter` | Apply a regex `GrepFilter` to a set of log lines; verify correct matches. |
| `test_json_field_filter` | Apply a `JsonFieldFilter` to a set of JSON log lines; verify correct field extraction and matching, including nested dot-paths. |
| `test_record_skip` | Write records of varying sizes; verify that a reader can skip records using `record_length` without parsing content. |
| `test_partial_line_flush` | Simulate EOF mid-line; verify the partial line is flushed with the truncation marker. |

### 10.2 Integration Tests

| Test | Description |
|------|-------------|
| `test_capture_from_container` | Start a container via Grill that emits known log lines on stdout and stderr. Verify that Ketchup captures all lines in the correct order with correct stream labels. |
| `test_day_rotation` | Simulate a day boundary (mock the clock). Verify that a new `.log` and `.idx` file are created, and that entries land in the correct day file based on their timestamp. |
| `test_compression_lifecycle` | Write log files with dates > `retention_days` ago. Run the maintenance task. Verify `.log.zst` files are created, original `.log` files are deleted, `.idx` files are preserved. |
| `test_retention_eviction` | Fill log storage to `max_storage`. Write additional logs. Verify that the oldest compressed files are evicted first, then the oldest raw files, and that today's files are never evicted. |
| `test_export_to_s3` | Configure export to a local MinIO instance. Write log entries. Run the export task. Verify that the correct `jsonl.gz` files appear in MinIO with the expected content. |
| `test_export_checkpoint` | Run an export. Write more entries. Run another export. Verify that only the new entries are exported (no duplicates). |
| `test_cross_node_query` | Start a 3-node cluster. Deploy an app with 3 replicas (one per node). Write distinct log lines from each replica. Query via `relish logs <app>`. Verify that all lines from all nodes appear in timestamp order. |
| `test_bun_restart_reconnect` | Start a container emitting continuous output. Restart Bun. Verify that capture resumes and no lines are lost (within the pipe buffer size). |

### 10.3 Property-Based Tests

| Test | Description |
|------|-------------|
| `prop_index_lookup_monotonic` | For any sorted sequence of timestamps and any query timestamp, `LogIndex::lookup()` returns an offset that is <= the offset of the first record with timestamp >= the query. |
| `prop_no_line_loss` | For any sequence of lines written to a capture pipe (up to pipe buffer size), all lines appear in the log file after the capture task processes them. |
| `prop_compression_roundtrip` | For any `.log` file, compressing to `.log.zst` and then querying the same time range returns identical results to querying the uncompressed file. |

### 10.4 Stress Tests

| Test | Description |
|------|-------------|
| `stress_500_apps` | Start 500 apps each emitting 200 lines/second. Verify Ketchup sustains capture without backpressure blocking any app for more than 10ms. Measure CPU and memory overhead. |
| `stress_large_lines` | Emit 1 MB log lines (e.g., base64 blobs). Verify correct capture and query without memory issues. |
| `stress_storage_exhaustion` | Set `max_storage = "100Mi"` and emit logs until exhaustion triggers. Verify eviction runs without data corruption and active files remain intact. |

---

## 11. Prior Art

### Kubernetes (kubelet log rotation)

In Kubernetes, container logs are written to files on the node by the container runtime (typically `/var/log/containers/<pod>_<namespace>_<container>-<id>.log`). The kubelet is responsible for log rotation based on size and file count limits. There is no built-in indexing, no structured query support, and no cross-node aggregation. `kubectl logs` reads from a single pod on a single node. Multi-node log aggregation requires a separate stack (Loki, EFK, Datadog).

**What Ketchup borrows:** The per-node, per-container log file model. Keeping logs local avoids the complexity and failure modes of a central log store.

**What Ketchup does differently:** Adds a timestamp index for fast time-range queries, automatic JSON detection for structured queries, built-in cross-node aggregation via the `relish logs` command, and automatic compression and export.

### Grafana Loki

[Loki](https://grafana.com/docs/loki/latest/get-started/architecture/) is a horizontally-scalable log aggregation system designed by Grafana Labs. It indexes logs by labels (app, namespace, node) rather than by full-text content, which makes it cheaper to operate than Elasticsearch. Logs are stored in chunks in object storage (S3, GCS) and queried via LogQL.

**What Ketchup borrows:** Label-based querying (filtering by app, instance, node, stream). The principle that full-text indexing is too expensive for logs and that label-based filtering with grep is sufficient for most use cases. The structured append-only storage model with separate indexes.

**What Ketchup does differently:** Ketchup is embedded per-node with no separate deployment. There is no central log store -- each node stores its own logs. Cross-node queries are fan-out queries to the source nodes, not queries against a central index. This eliminates the operational burden of running Loki (ingester, distributor, querier, compactor, object storage) but limits query performance for very large time ranges across many nodes.

### Elasticsearch + Fluentd + Kibana (EFK)

The [EFK stack](https://www.fluentd.org/architecture) is the traditional Kubernetes log aggregation solution. Fluentd runs as a DaemonSet on each node, tails container log files, and ships them to Elasticsearch. Kibana provides the query UI.

**What Ketchup borrows:** The DaemonSet-per-node collection model (Ketchup is the equivalent of Fluentd, but compiled into the agent rather than deployed separately).

**What Ketchup does differently:** No separate Elasticsearch cluster to manage (often the most operationally expensive component in a Kubernetes cluster). No Fluentd configuration (Ketchup captures automatically). No Kibana deployment. The tradeoff is that Ketchup does not provide full-text search indexing -- complex queries require grep-style scanning rather than inverted index lookups.

### Vector

[Vector](https://vector.dev/) by Datadog is a high-performance observability data pipeline written in Rust. It collects, transforms, and routes logs, metrics, and traces. Vector is a data router, not a storage engine -- it ships data to downstream systems.

**What Ketchup borrows:** The Rust-based, high-performance approach to log processing. The idea that log collection should have minimal overhead.

**What Ketchup does differently:** Ketchup includes storage and querying, not just collection and routing. Vector requires a downstream storage system; Ketchup is self-contained.

### Datadog Agent

Datadog's agent collects logs, metrics, and traces from each host and ships them to Datadog's SaaS platform for storage and analysis.

**What Ketchup borrows:** The single-agent-per-node model that captures all observability data.

**What Ketchup does differently:** All data stays on the node (unless explicitly exported). No SaaS dependency. No per-GB ingestion pricing. The tradeoff is that Ketchup's query capabilities are simpler than Datadog's full-text search and analytics.

---

## 12. Libraries and Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `tokio` | 1.x | Async runtime for capture tasks, query handlers, export tasks, and maintenance timers. Provides `AsyncBufReadExt` for line-oriented reading from container stdout/stderr pipes. |
| `datafusion` | 45 | SQL query engine over the on-disk Parquet log blocks. `--grep` is a SQL `LIKE` substring match. Parquet handles columnar (ZSTD) compression internally, so there is no separate compression dependency. |
| `object_store` | 0.14 | Reads and writes Parquet log blocks and ships exported archives behind a single interface over a local path, `file://`, `s3://`, or `gs://`. The built-in S3 backend removes the need for a dedicated AWS SDK. |
| `serde_json` | 1.x | JSON detection (parsing log lines to determine if they are structured JSON) and JSON field extraction for `--json-field` queries. Also used for serialising log records during export. |
| `tokio-util` | 0.7.x | `CancellationToken` for graceful shutdown of capture tasks when a workload stops. |

---

## 13. Open Questions

### 13.1 Multi-Line Log Detection

Many applications emit multi-line log entries (Java stack traces, Python tracebacks, formatted JSON blobs). Currently, Ketchup treats each `\n`-delimited line as a separate `LogEntry`. This means a Java stack trace appears as dozens of individual log lines with no grouping.

**Options under consideration:**

1. **Continuation heuristic:** Lines that start with whitespace or do not start with a recognizable timestamp pattern are considered continuations of the previous line. This is the approach used by Fluentd's `multiline` parser and works well for stack traces but fails for applications with inconsistent formatting.

2. **Application-declared pattern:** Allow apps to declare a `logs.multiline_pattern` regex that identifies the start of a new log entry. Lines not matching the pattern are appended to the previous entry. This is explicit and reliable but requires per-app configuration, violating the "zero configuration" goal.

3. **Do nothing (current approach):** Store each line separately. Multi-line grouping is a presentation concern handled by the CLI or UI (e.g., the Brioche log viewer could collapse indented continuation lines). This is the simplest approach and avoids buffering delays (multi-line detection requires holding lines until the next entry starts, introducing latency).

**Current leaning:** Option 3 (do nothing in the storage layer) with an optional `logs.multiline_start` per-app regex for applications that need it. The default experience stores each line separately; apps that declare a pattern get grouped entries.

### 13.2 Log Sampling for High-Volume Apps

Some applications (e.g., HTTP access logs for high-traffic APIs) produce millions of lines per day. At 500 bytes per line and 10 million lines per day, a single app produces ~5 GB/day of raw log data. With 30-day compressed retention at 10x compression, this is 1.5 GB of compressed storage per app -- feasible but significant.

**Options under consideration:**

1. **Rate limiting:** Drop lines when the per-app write rate exceeds a configurable threshold (e.g., 1000 lines/second). Dropped lines are counted. This is simple but loses potentially important data.

2. **Probabilistic sampling:** Keep 1-in-N lines when the rate exceeds a threshold. The sampling rate is recorded in the `LogEntry` so queries can extrapolate counts. This is useful for volume-oriented analysis but useless for debugging specific requests.

3. **Level-based sampling:** If `is_json = true` and the JSON contains a `level` field, keep all `error` and `warn` lines, sample `info` lines, and aggressively sample `debug`/`trace` lines. This preserves the most useful lines for debugging.

4. **Do nothing (current approach):** Store all lines. Rely on `max_storage` eviction to bound total usage. Let the operator configure per-app `retention_days` overrides for high-volume apps.

**Current leaning:** Option 4 (store everything, evict by age) with a future option for per-app rate limits. Sampling is complex to get right and surprising when a needed line was sampled away.

### 13.3 Real-Time Log Streaming Protocol

**Reconciliation note:** today `--follow` is local-node only (§3.7) -- there is no cross-node live merge. What ships for non-follow queries is an HTTP fan-out that runs each node's SQL query and merge-sorts the returned lists. The paragraph below describes the planned cross-node streaming design, not current behaviour.

The planned design uses streaming for cross-node log fan-out (the leader opens a stream to each node and merge-sorts the results). This targets the `relish logs --follow` use case but has limitations:

1. **Latency:** HTTP/2 framing adds overhead. A dedicated binary protocol over the existing inter-node mTLS connections could reduce tail latency for real-time streaming.

2. **Scalability:** For an app with 100 replicas across 50 nodes, the leader must maintain 50 concurrent streams. This is feasible but creates load on the leader node. A tree-based aggregation (similar to the metrics hierarchical aggregation via council members) could distribute the merge-sort load.

3. **Direct node connection:** For the common case of tailing a single instance (`relish logs web --instance web-3`), the leader could redirect the client directly to the node running that instance, avoiding the leader as a proxy entirely.

**Current leaning:** Start with HTTP/2 streaming (it is simple and correct). Add direct-to-node redirection for single-instance queries as an optimisation. Defer tree-based aggregation until cross-node log queries at scale (>50 nodes per query) prove to be a bottleneck.

### 13.4 Log Line Size Limit

Should Ketchup enforce a maximum line length? Unbounded line lengths create risks:

- A workload that accidentally logs a multi-GB binary blob could exhaust memory in the capture task's line buffer.
- Very long lines degrade query performance (every line must be scanned, even if the matching portion is in the first 100 bytes).

**Current leaning:** Default maximum line length of 64 KB. Lines exceeding the limit are truncated with a `[truncated at 65536 bytes]` suffix. The limit is configurable per-app via `logs.max_line_length`. This matches Docker's default log line limit and is sufficient for virtually all legitimate log output.

### 13.5 Structured Log Schema Registry

When `is_json = true`, Ketchup could optionally maintain a schema registry of observed JSON field names and types per-app. This would enable:

- Autocomplete in the Brioche UI for `--json-field` queries.
- Schema drift detection (new fields appearing, types changing).
- More efficient query planning (skip JSON parsing for lines that cannot match a field filter based on schema history).

**Current leaning:** Defer to a future iteration. The schema registry adds complexity and storage overhead. Start with parse-every-line for JSON field queries and optimise later if query performance becomes a problem.
