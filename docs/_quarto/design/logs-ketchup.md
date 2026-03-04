# Ketchup: Log Collection System

**Component:** Ketchup (Log Collector)
**Status:** Design Draft
**Whitepaper Reference:** Section 15

---

## 1. Overview

Ketchup is Reliaburger's built-in log collection subsystem. It automatically captures stdout and stderr from every managed container and process workload on a node, stores the output in structured append-only files organised per-app per-day, and provides querying, export, and retention management without any external dependencies.

Ketchup runs as a module within the Bun agent on every node. There is no separate log collection daemon, no sidecar container, and no central log aggregation server. Each node is responsible for capturing, indexing, compressing, and serving the logs produced by workloads running on that node. Cross-node queries are coordinated by the leader using the same fan-out pattern employed by Mayo for metrics: the query is dispatched only to nodes running the target app, and results are merged before being returned to the caller.

The design goals for Ketchup are:

- **Zero configuration by default.** Log capture is automatic for every workload. No log agent configuration, no log format specification, no output plugin selection.
- **Sub-millisecond per-app overhead.** At the design target of 500 apps per node, Ketchup must not become a bottleneck. Append-only writes with memory-mapped indexes keep per-line overhead in the low-microsecond range.
- **Structured query support.** When an application emits JSON-formatted log lines, Ketchup auto-detects this and enables field-level queries (e.g., `--json-field level=error`) without requiring the application to declare its log format.
- **Built-in retention and export.** Raw logs are retained for 7 days, compressed archives for 30 days. Long-term retention is handled by scheduled export to external destinations (S3, GCS, or any HTTP endpoint) in `jsonl.gz` format.

---

## 2. Dependencies

### Internal Dependencies

| Component | Dependency Type | Purpose |
|-----------|----------------|---------|
| **Bun** (Agent) | Host process | Ketchup runs as a module inside Bun. Bun provides the lifecycle hooks for container start/stop events and access to container stdio file descriptors. |
| **Grill** (Container Runtime Interface) | Stdio streams | Grill manages the container runtime (containerd/runc). When a container is created, Grill returns the stdout and stderr file descriptors (or named pipes) that Ketchup attaches to for log capture. For process workloads, Bun spawns the process with redirected stdout/stderr pipes that Ketchup reads from. |
| **Mustard** (Gossip) | Cluster topology | Ketchup uses the Mustard membership list to know which nodes are running which apps, enabling the leader to fan out cross-node log queries to the correct subset of nodes. |
| **Patty** (Scheduler) | App placement | The leader uses Patty's placement data to determine which nodes to query when a `relish logs` command targets an app with multiple instances across the cluster. |

### External Dependencies

None. Ketchup has no external dependencies for core functionality. Export destinations (S3, GCS, HTTP) are optional and configured by the operator.

---

## 3. Architecture

### 3.1 Capture Pipeline

```
Container/Process workload
    │
    ├─ stdout ──┐
    │            ├──→ Grill/Bun pipe ──→ Ketchup capture task
    └─ stderr ──┘                              │
                                               ├─ Parse timestamp (or assign wall clock)
                                               ├─ Detect JSON structure (first N lines)
                                               ├─ Wrap in LogEntry envelope
                                               ├─ Append to current day's log file
                                               └─ Update in-memory index
```

Each container or process workload has a dedicated Tokio task that reads from the stdout and stderr file descriptors. The task performs line-splitting (on `\n` boundaries), wraps each line in a `LogEntry` envelope with metadata, and appends it to the current day's log file for that app on that node.

The capture task is spawned when Bun starts a workload (or reconnects to a running workload after a Bun restart). It is cancelled when the workload stops. Backpressure is handled by the OS pipe buffer: if Ketchup cannot keep up with a workload's log output, the pipe buffer fills, and the workload's write to stdout/stderr blocks. This is the correct behaviour -- a workload that produces logs faster than the disk can absorb them should be throttled, not silently drop logs.

### 3.2 On-Disk File Layout

```
/var/lib/reliaburger/logs/
├── web/
│   ├── 2026-02-10.log          # Today's active log file (append-only)
│   ├── 2026-02-10.idx          # Memory-mapped timestamp index
│   ├── 2026-02-09.log          # Yesterday (still raw, within retention_days)
│   ├── 2026-02-09.idx
│   ├── 2026-02-03.log.zst      # Older file, compressed with zstd
│   ├── 2026-02-03.idx          # Index is NOT compressed (small, needed for queries)
│   └── ...
├── api/
│   ├── 2026-02-10.log
│   ├── 2026-02-10.idx
│   └── ...
├── payment-service/
│   └── ...
└── _events/                    # Cluster events (scheduling, health, deploys)
    ├── 2026-02-10.log
    ├── 2026-02-10.idx
    └── ...
```

**Naming convention:** `<app_name>/<YYYY-MM-DD>.log` and `<app_name>/<YYYY-MM-DD>.idx`. Compressed files add the `.zst` suffix to the log file only. Index files are never compressed because they are small (fixed-size records) and must be random-access readable for time-range queries.

**Day boundary rotation:** At midnight UTC, the capture task closes the current day's file handle and opens a new one. In-flight lines are guaranteed to land in the file matching the timestamp in their `LogEntry` envelope, not the file-open time. If a line arrives at 23:59:59.999 but is processed at 00:00:00.001, it goes into the previous day's file based on its captured timestamp.

### 3.3 Log File Format

Each `.log` file is a sequence of length-prefixed binary records. This is not a plain text file -- it is a structured append-only format that supports efficient seeking without parsing every line.

```
┌─────────────────────────────────────────────────────┐
│ Record:                                             │
│   [4 bytes]  record_length (u32 little-endian)      │
│   [8 bytes]  timestamp_nanos (u64 LE, Unix epoch)   │
│   [1 byte]   stream (0x01 = stdout, 0x02 = stderr)  │
│   [2 bytes]  instance_id (u16 LE)                   │
│   [1 byte]   flags (bit 0: is_json)                 │
│   [N bytes]  line (UTF-8, no trailing newline)      │
├─────────────────────────────────────────────────────┤
│ Record:                                             │
│   ...                                               │
└─────────────────────────────────────────────────────┘
```

The `record_length` field covers everything after itself (timestamp + stream + instance_id + flags + line). This allows a reader to skip records without parsing their contents, which is critical for fast seeking.

### 3.4 Timestamp Index

Each `.idx` file is a flat array of fixed-size index entries, memory-mapped for O(1) random access:

```
┌─────────────────────────────────────────────────┐
│ Index Entry (16 bytes):                         │
│   [8 bytes]  timestamp_nanos (u64 LE)           │
│   [8 bytes]  file_offset (u64 LE)               │
├─────────────────────────────────────────────────┤
│ Index Entry:                                    │
│   ...                                           │
└─────────────────────────────────────────────────┘
```

One index entry is written for every N log records (default N=64, configurable). This provides a sparse index: to find logs at a given timestamp, binary search the index to find the nearest preceding entry, then scan forward from that file offset. With 64 records per index entry, the index for a file containing 10 million log lines is ~2.4 MB (10M / 64 * 16 bytes), which fits comfortably in memory via `mmap`.

Index entries are always sorted by timestamp (log records are appended in timestamp order because each capture task processes lines sequentially per-workload, and the index is append-only).

### 3.5 JSON Auto-Detection

When a capture task starts (or when Bun reconnects to a running workload), Ketchup examines the first 20 lines from stdout. If all 20 lines are valid JSON objects (parse successfully with `serde_json`), the `is_json` flag is set on all subsequent records for that stream. This flag is sticky per-stream per-workload-lifecycle: once set, it persists until the workload restarts. If a workload emits a mix of JSON and non-JSON lines, the flag is not set, and structured queries fall back to text matching.

The detection window of 20 lines is a tradeoff: large enough to avoid false positives from a single JSON-formatted startup banner, small enough to complete detection quickly. The detection result is logged as a Ketchup-internal event: `ketchup: JSON detection for app=web instance=web-3 stream=stdout result=true`.

### 3.6 Compression Pipeline

A background Tokio task runs once per hour (configurable) and compresses log files that are older than `retention_days` but younger than `compressed_retention_days`. Compression uses `zstd` at level 3 (the default, which provides a good balance of speed and ratio for log data). The compression pipeline:

1. Identify `.log` files with date stamps older than `retention_days` that do not yet have a `.log.zst` counterpart.
2. Open the `.log` file read-only.
3. Compress to a temporary file `<date>.log.zst.tmp` using streaming zstd compression.
4. `fsync` the temporary file.
5. Rename `<date>.log.zst.tmp` to `<date>.log.zst` (atomic on POSIX).
6. Delete the original `.log` file.

The `.idx` file is retained uncompressed. For queries against compressed files, Ketchup decompresses the relevant portion of the `.zst` file on-the-fly using zstd's seekable frame format -- the file is compressed in 256 KB frames, allowing random access to any frame without decompressing the entire file.

### 3.7 Cross-Node Query Architecture

```
relish logs web --since 1h --grep "ERROR"
    │
    ▼
Leader receives query
    │
    ├─ Lookup: which nodes run app "web"?
    │   (from Patty placement state: node-01, node-02, node-03)
    │
    ├─ Fan out LogQuery to node-01, node-02, node-03 in parallel
    │
    ▼
Each node's Ketchup:
    ├─ Open index for today's file
    ├─ Binary search for --since timestamp
    ├─ Scan forward, applying --grep filter
    ├─ Stream matching LogEntry records back to leader
    │
    ▼
Leader:
    ├─ Merge-sort results by timestamp (from all nodes)
    └─ Stream to client (relish CLI or Brioche UI)
```

For `relish logs web` with no time range (tail mode), the leader opens a streaming connection to all nodes running the app. Each node's Ketchup tails its active log file and pushes new entries as they are written. The leader merge-sorts the streams by timestamp and forwards to the client. This provides a unified real-time log view across all instances, similar to `stern` in the Kubernetes ecosystem but without requiring a separate tool.

---

## 4. Data Structures

### 4.1 Core Types

```rust
use std::path::PathBuf;
use std::time::Duration;

/// A single log line captured from a workload's stdout or stderr.
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Nanosecond-precision Unix timestamp of when the line was captured.
    pub timestamp_nanos: u64,
    /// The app this log line belongs to (e.g., "web", "api").
    pub app: String,
    /// The specific instance within the app (e.g., "web-3").
    pub instance: String,
    /// The node where this log line was captured.
    pub node: String,
    /// Which output stream this line came from.
    pub stream: LogStream,
    /// The raw log line content (UTF-8, no trailing newline).
    pub line: String,
    /// Whether this line was detected as valid JSON.
    pub is_json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LogStream {
    Stdout = 0x01,
    Stderr = 0x02,
}

/// A single entry in the sparse timestamp index.
/// Fixed-size (16 bytes) for memory-mapped random access.
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct IndexEntry {
    /// Nanosecond-precision Unix timestamp.
    pub timestamp_nanos: u64,
    /// Byte offset into the corresponding .log file.
    pub file_offset: u64,
}

/// Represents a memory-mapped index file for fast timestamp lookups.
pub struct LogIndex {
    /// Memory-mapped view of the .idx file.
    mmap: memmap2::Mmap,
    /// Number of entries in the index.
    entry_count: usize,
    /// Path to the index file (for error reporting).
    path: PathBuf,
}

impl LogIndex {
    /// Binary search for the index entry whose timestamp is <= the target.
    /// Returns the file offset to start scanning from.
    pub fn lookup(&self, timestamp_nanos: u64) -> Option<u64> {
        // Binary search over the fixed-size entries in the mmap.
        // Each entry is 16 bytes: 8 bytes timestamp + 8 bytes offset.
        let entries = self.as_slice();
        let pos = entries.partition_point(|e| e.timestamp_nanos <= timestamp_nanos);
        if pos == 0 {
            return Some(0); // Before all indexed entries; start from beginning.
        }
        Some(entries[pos - 1].file_offset)
    }

    fn as_slice(&self) -> &[IndexEntry] {
        // Safety: IndexEntry is repr(C, packed), 16 bytes, no padding.
        unsafe {
            std::slice::from_raw_parts(
                self.mmap.as_ptr() as *const IndexEntry,
                self.entry_count,
            )
        }
    }
}
```

### 4.2 Query Types

```rust
/// A log query received from the CLI, API, or Brioche UI.
#[derive(Debug, Clone)]
pub struct LogQuery {
    /// Target app name (required).
    pub app: String,
    /// Optional instance filter (e.g., "web-3").
    pub instance: Option<String>,
    /// Optional node filter.
    pub node: Option<String>,
    /// Stream filter (stdout, stderr, or both).
    pub stream: Option<LogStream>,
    /// Start of the time range. None means "from the beginning of retained logs."
    pub since: Option<u64>,
    /// End of the time range. None means "up to now" (or "follow" if tailing).
    pub until: Option<u64>,
    /// Text substring or regex pattern to match against the log line.
    pub grep: Option<GrepFilter>,
    /// Structured JSON field query (e.g., level=error).
    pub json_field: Option<Vec<JsonFieldFilter>>,
    /// Maximum number of lines to return. None means unlimited (streaming).
    pub limit: Option<usize>,
    /// Whether to follow (tail -f mode): keep the connection open and stream
    /// new lines as they are written.
    pub follow: bool,
}

#[derive(Debug, Clone)]
pub struct GrepFilter {
    /// The pattern string.
    pub pattern: String,
    /// Compiled regex (compiled once at query parse time).
    pub regex: regex::Regex,
    /// Case-insensitive flag.
    pub case_insensitive: bool,
}

#[derive(Debug, Clone)]
pub struct JsonFieldFilter {
    /// Dot-separated JSON field path (e.g., "level" or "request.method").
    pub field_path: String,
    /// Expected value (string comparison after JSON extraction).
    pub value: String,
}
```

### 4.3 Export and Retention Configuration

```rust
/// Configuration for log export to an external destination.
#[derive(Debug, Clone, Deserialize)]
pub struct LogExportConfig {
    /// Destination URI (e.g., "s3://my-bucket/logs/", "gs://my-bucket/logs/",
    /// "https://logs.example.com/ingest").
    pub destination: String,
    /// Output format. Currently only "jsonl.gz" is supported.
    pub format: ExportFormat,
    /// How often to run the export (e.g., "1h", "6h", "24h").
    pub interval: Duration,
    /// Optional: only export logs for specific apps. None means all apps.
    pub apps: Option<Vec<String>>,
    /// Optional: include or exclude specific fields in the export.
    pub fields: Option<ExportFieldSelection>,
}

#[derive(Debug, Clone, Deserialize)]
pub enum ExportFormat {
    /// Newline-delimited JSON, gzip-compressed. Each line is a full LogEntry
    /// serialised as JSON.
    #[serde(rename = "jsonl.gz")]
    JsonlGz,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExportFieldSelection {
    /// Fields to include. If set, only these fields appear in the export.
    pub include: Option<Vec<String>>,
    /// Fields to exclude. Applied after include.
    pub exclude: Option<Vec<String>>,
}

/// Retention policy for logs on a node.
#[derive(Debug, Clone, Deserialize)]
pub struct RetentionPolicy {
    /// Number of days to keep raw (uncompressed) log files. Default: 7.
    pub retention_days: u32,
    /// Number of days to keep compressed log files. Default: 30.
    /// After this period, compressed files are deleted.
    pub compressed_retention_days: u32,
    /// Maximum total storage for logs on this node. When exceeded,
    /// the oldest compressed files are evicted first, then the oldest raw files.
    pub max_storage: ByteSize,
}

/// Parsed byte size (e.g., "20Gi" -> 21474836480 bytes).
#[derive(Debug, Clone, Deserialize)]
pub struct ByteSize(pub u64);
```

### 4.4 Internal Capture State

```rust
/// Per-workload capture state maintained by the Ketchup module inside Bun.
pub struct CaptureHandle {
    /// The app this capture belongs to.
    pub app: String,
    /// The instance name (e.g., "web-3").
    pub instance: String,
    /// Instance ID (u16, used in the binary log record for compact encoding).
    pub instance_id: u16,
    /// Handle to the Tokio task reading stdout.
    pub stdout_task: tokio::task::JoinHandle<()>,
    /// Handle to the Tokio task reading stderr.
    pub stderr_task: tokio::task::JoinHandle<()>,
    /// Whether JSON auto-detection has completed for stdout.
    pub stdout_json_detected: bool,
    /// Whether JSON auto-detection has completed for stderr.
    pub stderr_json_detected: bool,
    /// Cancellation token for graceful shutdown.
    pub cancel: tokio_util::sync::CancellationToken,
}

/// Writer state for a single app's log file on a single day.
pub struct DayLogWriter {
    /// The app name.
    pub app: String,
    /// The date this writer covers (YYYY-MM-DD).
    pub date: chrono::NaiveDate,
    /// File handle for the .log file, opened in append mode.
    pub log_file: tokio::fs::File,
    /// File handle for the .idx file, opened in append mode.
    pub idx_file: tokio::fs::File,
    /// Number of records written since the last index entry.
    pub records_since_last_index: u32,
    /// The index interval (write an index entry every N records).
    pub index_interval: u32,
    /// Current byte offset in the log file (tracked in-memory to avoid seeks).
    pub current_offset: u64,
    /// Mapping from instance name to instance_id for this day.
    pub instance_ids: HashMap<String, u16>,
    /// Next available instance_id.
    pub next_instance_id: u16,
}
```

---

## 5. Operations

### 5.1 Log Capture

**Container workloads:** When Grill starts a container, it returns the stdout and stderr file descriptors (Unix pipes). Bun passes these to Ketchup, which spawns two Tokio tasks (one per stream) via `tokio::io::BufReader::new(AsyncFd::new(fd))`. Each task reads lines using `AsyncBufReadExt::read_line()`, wraps them in a `LogEntry`, and sends them to the `DayLogWriter` for the current day via an `mpsc` channel.

**Process workloads:** Bun spawns the process with `tokio::process::Command` using `stdout(Stdio::piped())` and `stderr(Stdio::piped())`. The resulting `ChildStdout` and `ChildStderr` handles are passed to Ketchup in the same way as container file descriptors.

**Reconnection after Bun restart:** When Bun restarts (e.g., during a self-upgrade), it queries the container runtime to discover running containers and reconnects to their stdout/stderr streams. Ketchup resumes capture from the current position in the stream. Log lines emitted while Bun was restarting are buffered in the OS pipe (default 64 KB on Linux). If the pipe buffer overflows (workload blocked on write), the lines are still not lost -- they are delivered when the capture task resumes. There is a gap in timestamps corresponding to the Bun restart duration, but no log lines are dropped.

### 5.2 Structured JSON Detection

The detection algorithm per-stream per-workload:

```
1. Collect the first 20 lines from the stream into a detection buffer.
2. For each line, attempt serde_json::from_str::<serde_json::Value>(&line).
3. If all 20 lines parse as JSON objects (Value::Object), set is_json = true.
4. If any line fails to parse, or parses as a non-object type, set is_json = false.
5. Flush the detection buffer to the log file (with the determined is_json flag).
6. For all subsequent lines, use the determined is_json flag without re-checking.
```

The detection buffer introduces a maximum latency of 20 lines before the first log output appears for a newly started workload. For workloads that emit fewer than 20 lines and then go quiet, a timeout of 5 seconds triggers early detection with whatever lines are available (if all N < 20 lines are JSON objects, `is_json = true`; if N = 0, `is_json = false`).

### 5.3 Querying

All query operations are initiated via `relish logs` (CLI), the Brioche UI, or the HTTP API.

**Tail (default mode):**
```bash
relish logs web
```
Opens a streaming follow connection. The leader fans the query to all nodes running `web`. Each node tails its active log file using `inotify` (Linux) to detect new writes and streams new entries. The leader merge-sorts by timestamp and streams to the client. The client displays lines as they arrive, prefixed with instance name and stream indicator.

**Time range query:**
```bash
relish logs web --since 1h
relish logs web --since "2026-02-10T14:00:00Z" --until "2026-02-10T15:00:00Z"
```
Each node uses the `LogIndex::lookup()` to binary-search the index for the start timestamp, then scans forward to the end timestamp. For queries spanning multiple days, Ketchup opens the relevant day files in sequence. For compressed files, Ketchup uses zstd seekable decompression to jump to the correct frame.

**Text search (grep):**
```bash
relish logs web --grep "ERROR"
relish logs web --grep "user_id=abc.*timeout" --since 24h
```
The `--grep` pattern is compiled to a `regex::Regex` at query time and applied to each log line during the scan. The regex is applied after the time-range filter (index lookup first, then scan + filter). Case-insensitive matching is available via `--grep-i`.

**Structured JSON field queries:**
```bash
relish logs web --json-field level=error
relish logs web --json-field "request.status=500" --json-field "request.method=POST"
```
For lines where `is_json = true`, the line is parsed with `serde_json` and the specified field path is extracted using dot-notation traversal. The extracted value is compared as a string. Multiple `--json-field` filters are ANDed. Lines where `is_json = false` are skipped (not matched) when a `--json-field` filter is active.

**Instance filter:**
```bash
relish logs web --instance web-3
```
Filters records by the `instance_id` field in the binary record. The instance name-to-ID mapping is stored in the `DayLogWriter` and reconstructed from the log file header on query (the first occurrence of each instance_id in the file establishes the mapping).

**Combined filters:**
```bash
relish logs web --instance web-3 --since 1h --grep "ERROR" --json-field level=error
```
Filters are applied in order of cheapness: time range (index lookup) -> instance_id (field comparison) -> is_json flag check -> JSON field extraction -> grep regex. This minimizes the number of lines that require expensive operations.

### 5.4 Log Export

Export runs as a periodic background task on each node, configured via:

```toml
[logs.export]
destination = "s3://my-bucket/logs/"
format = "jsonl.gz"
interval = "1h"
```

**Export pipeline:**

1. Every `interval`, the export task identifies log files that have been modified since the last export checkpoint.
2. For each file, it reads records from the last-exported offset to the current end of file.
3. Each record is serialised to a JSON line (the full `LogEntry` structure) and written to a gzip-compressed output buffer.
4. The output is uploaded to the destination. For S3/GCS, the upload uses multipart upload with 8 MB parts to handle large batches. For HTTP endpoints, the body is POST-ed as a gzip-compressed `application/x-ndjson` stream.
5. On successful upload, the export checkpoint is advanced to the current offset.

**Export file naming at the destination:**
```
s3://my-bucket/logs/<node>/<app>/<YYYY-MM-DD>/<HH>.jsonl.gz
```
One file per app per node per hour, so downstream consumers can easily partition and query by time.

**Export checkpoint persistence:** The last-exported offset per app per day file is stored in a small SQLite database at `/var/lib/reliaburger/logs/_export_state.db`. This survives Bun restarts and ensures exactly-once export semantics (modulo destination-side deduplication for retries after upload timeout).

### 5.5 Retention Management

A background task runs once per hour and enforces the retention policy:

1. **Delete expired compressed files:** Any `.log.zst` file with a date older than `compressed_retention_days` ago is deleted, along with its `.idx` file.
2. **Compress aging raw files:** Any `.log` file with a date older than `retention_days` ago (but within `compressed_retention_days`) is compressed to `.log.zst` and the original is deleted.
3. **Enforce max_storage:** If the total size of all files under `/var/lib/reliaburger/logs/` exceeds `max_storage`, the oldest compressed files are evicted first (oldest date first, across all apps). If compressed files are exhausted and storage is still over the limit, the oldest raw files are evicted. Active (today's) files are never evicted.

Eviction events are logged as cluster events visible in `relish events --type log-retention`.

### 5.6 Cross-Node Log Aggregation

The `relish logs` command provides a unified view across all nodes. The aggregation flow:

1. The CLI sends a `LogQuery` to the cluster leader via the HTTP API.
2. The leader looks up which nodes run the target app (from the Raft-stored placement state).
3. The leader opens concurrent gRPC streams (or HTTP/2 streams) to each relevant node's Ketchup endpoint.
4. Each node's Ketchup executes the query locally and streams matching `LogEntry` records.
5. The leader performs a k-way merge sort on the incoming streams, ordered by `timestamp_nanos`.
6. Merged entries are streamed back to the CLI (or Brioche UI) as newline-delimited JSON over the HTTP response.

For follow/tail mode, the streams remain open and new entries are pushed as they arrive. The merge maintains a small buffer (100ms window) to handle clock skew between nodes before emitting entries to the client.

---

## 6. Configuration

All configuration is in the node's TOML config file under the `[logs]` section.

```toml
# ── Logs (Ketchup) ─────────────────────────────────
[logs]
# Number of days to keep raw (uncompressed) log files.
# After this period, files are compressed with zstd.
# Default: 7
retention_days = 7

# Number of days to keep compressed log files.
# After this period, compressed files are deleted.
# Default: 30
compressed_retention_days = 30

# Maximum total disk space for log storage on this node.
# When exceeded, oldest files are evicted (compressed first, then raw).
# Default: "20Gi"
max_storage = "20Gi"

# Sparse index interval: one index entry per N log records.
# Lower values = faster time-range lookups, larger index files.
# Default: 64
index_interval = 64

# Zstd compression level for archived log files (1-22).
# Level 3 is the default, offering good compression ratio with fast speed.
# Default: 3
compression_level = 3

# Zstd seekable frame size in bytes. Larger frames = better compression ratio,
# smaller frames = faster random access into compressed files.
# Default: 262144 (256 KB)
compression_frame_size = 262144

# How often to run the compression and retention cleanup task.
# Default: "1h"
maintenance_interval = "1h"

# ── Log Export (optional) ──────────────────────────
[logs.export]
# Destination URI for log export.
# Supported schemes: s3://, gs://, https://
# No default (export is disabled if not set).
destination = "s3://my-bucket/logs/"

# Export file format. Currently only "jsonl.gz" is supported.
# Default: "jsonl.gz"
format = "jsonl.gz"

# How often to run the export.
# Default: "1h"
interval = "1h"

# Optional: only export logs for these apps.
# Default: all apps.
# apps = ["web", "api"]

# ── Per-App Overrides (in the app definition) ──────
# [app.noisy-worker]
# image = "worker:latest"
# logs.retention_days = 3          # Override: keep raw logs for only 3 days
# logs.max_line_length = 4096      # Truncate lines longer than 4 KB
# logs.suppress_stderr = false     # Default: capture both streams
```

### Configuration Defaults and Rationale

| Parameter | Default | Rationale |
|-----------|---------|-----------|
| `retention_days` | 7 | Covers a typical on-call rotation. Most debugging happens within hours, but a week of raw logs enables post-incident review. |
| `compressed_retention_days` | 30 | Compressed logs are ~10x smaller; a month of history is cheap to store. |
| `max_storage` | 20 Gi | Prevents runaway log growth from consuming disk needed by the container runtime and application volumes. |
| `index_interval` | 64 | At 64 records per index entry, the index is ~0.025% the size of the log data. A query for a specific second in a 10M-line file requires scanning at most 64 records after the index lookup. |
| `compression_level` | 3 | Zstd level 3 compresses log data at ~400 MB/s with a typical ratio of 5-10x. Higher levels offer diminishing returns for log data. |

---

## 7. Failure Modes

### 7.1 Storage Exhaustion

**Trigger:** Log output exceeds `max_storage` or the underlying filesystem fills up.

**Detection:** The retention task checks total log storage every maintenance interval. Additionally, Ketchup monitors `write()` return values on every log append -- an `ENOSPC` error triggers immediate eviction.

**Response:**

1. Emergency eviction: delete the oldest compressed files across all apps until storage drops below 90% of `max_storage`.
2. If still over limit after all compressed files are deleted, delete the oldest raw files (except today's active files).
3. If today's active files alone exceed `max_storage`, emit a `critical` alert (`log.storage_exhausted`) and begin dropping log lines from the highest-volume app (tracked by a per-app byte counter). Dropped lines are counted but not stored; the count is visible in `relish events`.
4. The `disk.filling` alert from Mayo also fires independently, alerting the operator to the underlying disk pressure.

**Invariant:** Ketchup never causes a node to become unhealthy due to disk exhaustion. Log data is the lowest-priority data on the node (below container images, application volumes, metrics, and Raft state).

### 7.2 Container Restart During Capture

**Trigger:** A container is killed (OOM, health check failure, deploy) while the capture task is reading its stdout/stderr.

**Response:** The capture task's `read_line()` call returns `Ok(0)` (EOF) or `Err(BrokenPipe)`. The task logs the termination reason (if available from Grill), flushes any partial line in its buffer (with a `[truncated]` marker if the line lacks a trailing newline), and exits. When the new container instance starts, a new capture task is spawned.

**Partial line handling:** If the container is killed mid-line (no `\n` before EOF), the partial content is flushed as a complete record with an `is_truncated` flag. This prevents silent data loss for the last line of output before a crash -- which is often the most important line for debugging.

### 7.3 Corrupt Index

**Trigger:** Power loss or kernel panic during an index write leaves a truncated or corrupt `.idx` file.

**Detection:** On startup (or when opening an index for a query), Ketchup validates that the file size is a multiple of 16 bytes (the `IndexEntry` size). If not, the file is truncated to the nearest valid boundary. Additionally, the last entry's `file_offset` is validated against the actual `.log` file size.

**Recovery:** If the index is corrupt beyond simple truncation (e.g., entries are not monotonically increasing in timestamp), Ketchup rebuilds the index by scanning the `.log` file from the beginning. This is O(N) in the number of records but only happens on corruption, not on normal startup. A rebuild event is logged.

**Prevention:** Index entries are written with a single `write()` syscall (16 bytes, well within the atomic write guarantee for regular files on Linux). Combined with the append-only nature of the file, corruption requires a kernel-level failure, not an application bug.

### 7.4 Clock Skew

**Trigger:** The system clock jumps backward (NTP correction, VM migration).

**Response:** Ketchup uses monotonic timestamps for ordering within a single capture session and wall-clock timestamps for the `LogEntry` envelope. If the wall clock jumps backward, the index may contain non-monotonic entries for a brief period. The query engine handles this gracefully: the binary search finds an approximate position, and the forward scan skips entries outside the requested range. Cross-node merge sort tolerates clock skew via the 100ms buffering window.

### 7.5 High-Volume Log Flood

**Trigger:** A workload emits log lines at a rate that exceeds the disk write bandwidth.

**Response:** OS pipe backpressure throttles the workload's stdout/stderr writes. This is intentional -- a workload that logs faster than the system can handle should be slowed down, not allowed to silently drop logs. If the operator prefers to drop logs rather than slow the workload, a future `logs.drop_policy = "tail"` option could allow Ketchup to discard lines from its read buffer when write throughput is insufficient (see Open Questions).

---

## 8. Security Considerations

### 8.1 Log Access Control

Log access is governed by Reliaburger's existing permission model (Sesame). The relevant permissions:

| Permission | Scope | Grants |
|------------|-------|--------|
| `logs:read` | Per-app or cluster-wide | Read log lines for the specified app(s). |
| `logs:export` | Cluster-wide | Configure and trigger log export. |
| `logs:admin` | Cluster-wide | Modify retention policies, delete log files. |

By default, the `admin` role has all log permissions. The `developer` role has `logs:read` for apps in their namespace. The `viewer` role has `logs:read` with a configurable line limit (default: last 1000 lines) to prevent bulk exfiltration via the query API.

**API enforcement:** Every `LogQuery` arriving at a node's Ketchup endpoint carries the authenticated identity of the requester (from the mTLS certificate or API token). Ketchup verifies that the requester has `logs:read` permission for the target app before executing the query. Cross-node fan-out queries carry the original requester's identity, not the leader's identity, so permission checks are enforced at the data source.

### 8.2 Sensitive Data in Logs

Ketchup does **not** perform automatic redaction of sensitive data in log lines. This is a deliberate design choice: automatic redaction is unreliable (false positives corrupt debugging data, false negatives provide a false sense of security) and should be the responsibility of the application.

However, Ketchup provides two mechanisms for operators who need log data controls:

1. **Export field filtering:** The `[logs.export]` config supports `fields.exclude` to strip specific JSON fields before export. This allows operators to export logs to external systems without including fields like `user.email` or `request.headers.authorisation`.

2. **Log scrubbing jobs:** Operators can define a Job that runs periodically and scrubs specific patterns from log files on disk. This is a power-user feature and is not recommended for most deployments (it modifies append-only files, which invalidates indexes and requires a rebuild).

**Recommendation in documentation:** Applications should avoid logging secrets, tokens, passwords, and PII. Ketchup treats log lines as opaque data and stores them as-is.

### 8.3 Log File Permissions

On-disk log files are owned by the Bun process user (typically `reliaburger` or `root`) with mode `0640`. The group is set to a configurable `log_group` (default: same as the Bun process group). Containers cannot access log files directly -- they are outside the container's mount namespace. Access is only available via the Ketchup query API (which enforces permissions) or direct host filesystem access (which requires host-level privileges).

---

## 9. Performance

### 9.1 Write Throughput

**Target:** Sustain capture from 500 concurrent apps per node with a combined log output of 100,000 lines/second (200 lines/second per app average) without dropping lines or introducing visible latency.

**Mechanism:** Each capture task writes to the `DayLogWriter` via a bounded `mpsc` channel (capacity: 8192 entries). The `DayLogWriter` batches pending entries and issues a single `writev()` syscall for multiple records, amortizing the syscall overhead. With an average log line of 200 bytes, the binary record overhead is 16 bytes (header), yielding ~216 bytes per record. At 100,000 lines/second, this is ~20.6 MB/s of sustained write throughput -- well within the capability of any modern SSD and even many spinning disks.

**Buffered I/O:** Log files are opened with `O_APPEND` and writes use a 64 KB userspace buffer (flushed on buffer full or every 100ms, whichever comes first). The 100ms flush interval bounds the maximum data loss on a process crash to 100ms of log output per app. The `fsync` is **not** called on every flush (this would destroy write throughput); instead, `fsync` is called once per minute. This means a kernel panic could lose up to 1 minute of log data, which is an acceptable tradeoff for the write throughput gain.

### 9.2 Query Latency

**Time-range query:** For a query over a 1-hour window in a file with 10M lines:

1. Index binary search: O(log(N/64)) = O(log(156250)) ~ 17 comparisons. With memory-mapped index, this completes in < 1 microsecond.
2. Forward scan from the index entry: at most 64 records to reach the start of the time window, then sequential scan through the matching records.
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
| `zstd` | 0.13.x | Compression and decompression of archived log files. Used in streaming mode for the compression pipeline and in seekable-frame mode for random access into compressed files during queries. |
| `serde_json` | 1.x | JSON detection (parsing log lines to determine if they are structured JSON) and JSON field extraction for `--json-field` queries. Also used for serialising `LogEntry` records to JSONL format during export. |
| `memmap2` | 0.9.x | Memory-mapped file access for `.idx` files. Provides zero-copy read access to the sparse timestamp index, enabling O(1) access to any index entry without loading the entire file into heap memory. |
| `regex` | 1.x | Compiled regular expression matching for `--grep` queries. The `regex` crate uses SIMD-accelerated DFA construction for high-throughput matching (~1 GB/s on modern CPUs). |
| `chrono` | 0.4.x | Date and time handling for day-boundary rotation, retention calculation, and timestamp parsing in `--since`/`--until` arguments. |
| `tokio-util` | 0.7.x | `CancellationToken` for graceful shutdown of capture tasks when a workload stops. `ReusableBoxFuture` for efficient task reuse in the capture pipeline. |
| `bytes` | 1.x | Zero-copy byte buffer management for the write pipeline. `BytesMut` is used to construct binary log records before writing to disk. |
| `notify` (or `inotify`) | 7.x | Filesystem event notification for tail/follow mode. Ketchup watches the active log file for new writes and pushes new entries to streaming query clients. On Linux, this uses `inotify`; on macOS (dev environments), `kqueue`. |
| `aws-sdk-s3` (optional) | 1.x | S3 upload for log export. Compiled behind a `feature = "export-s3"` flag to avoid pulling in the AWS SDK for clusters that don't use S3 export. |

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

The current design uses HTTP/2 streaming for cross-node log fan-out (the leader opens a stream to each node and merge-sorts the results). This works for the `relish logs --follow` use case but has limitations:

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
