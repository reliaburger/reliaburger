//! Arrow/DataFusion-based log store.
//!
//! Logs are buffered in memory, periodically flushed to Parquet, and
//! queryable via DataFusion SQL. Mirrors the MayoStore architecture
//! exactly — same engine for both metrics and logs.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use datafusion::arrow::array::UInt64Array;
use datafusion::arrow::array::{Array, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::MemTable;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::parquet::basic::{Compression, ZstdLevel};
use datafusion::parquet::file::properties::WriterProperties;
use datafusion::prelude::*;

use super::types::{KetchupError, LogEntry, LogStream};

/// Escape a value for safe interpolation into a single-quoted SQL string
/// literal (M1): a `'` is doubled, per standard SQL / DataFusion. Prevents
/// a log query param from breaking out of the literal to read other
/// tenants' logs.
pub(crate) fn escape_sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}

/// Maximum rows the bounded raw-log SQL endpoint returns (OBS5).
///
/// The `/v1/logs/sql` endpoint wraps the caller's query in an outer `LIMIT`
/// of this, so an unbounded `SELECT * FROM logs` can't stream the whole
/// archive back through one response.
pub const MAX_LOG_SQL_ROWS: usize = 10_000;

/// Working-memory limit for a bounded raw-log SQL query (OBS5): 256 MiB.
///
/// A query that would need more than this to sort or aggregate errors rather
/// than exhausting the agent's memory.
pub const LOG_SQL_MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// Convert collected RecordBatches to JSON objects (one per row), mapping
/// `UInt64`/`Utf8` columns to numbers/strings.
fn batches_to_json(batches: &[RecordBatch]) -> Vec<serde_json::Value> {
    let mut results = Vec::new();
    for batch in batches {
        let schema = batch.schema();
        for row in 0..batch.num_rows() {
            let mut obj = serde_json::Map::new();
            for (col_idx, field) in schema.fields().iter().enumerate() {
                let col = batch.column(col_idx);
                let value = if let Some(arr) = col.as_any().downcast_ref::<UInt64Array>() {
                    serde_json::Value::Number(arr.value(row).into())
                } else if let Some(arr) = col
                    .as_any()
                    .downcast_ref::<datafusion::arrow::array::Int64Array>()
                {
                    // COUNT(*) and other aggregates come back as Int64.
                    serde_json::Value::Number(arr.value(row).into())
                } else if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
                    serde_json::Value::String(arr.value(row).to_string())
                } else {
                    serde_json::Value::String(format!("{:?}", col))
                };
                obj.insert(field.name().clone(), value);
            }
            results.push(serde_json::Value::Object(obj));
        }
    }
    results
}

/// Rows per Parquet row group for flushed log files.
///
/// Logs are written in small row groups so that row-group statistics and
/// bloom filters can skip irrelevant groups during archive queries. A
/// flush of a few thousand lines then spans several groups rather than one
/// monolithic block.
const LOG_ROW_GROUP_SIZE: usize = 8192;

/// Hard ceiling on unflushed log entries (M8). If flushing keeps failing (e.g.
/// disk full — logged every 60s by the flush task) the buffer would otherwise
/// grow without bound while containers keep logging, eventually OOM-ing the
/// node. At the cap the oldest entries are dropped: shedding the tail of the
/// log backlog is the lesser evil compared with killing the whole node.
/// RollupStore bounds itself the same way.
const MAX_BUFFER_ROWS: usize = 1_000_000;

/// Parquet writer properties for flushed log files.
///
/// Two optimisations, both of which only matter for the *archive* read
/// path (`relish logs-search` over exported Parquet), never the in-memory
/// hot path:
///
/// - **ZSTD compression** on every column chunk. Repetitive log lines
///   compress hard, and because Parquet compresses per row group the file
///   stays randomly accessible — any group decompresses on its own.
/// - **Bloom filters on `app` and `namespace`.** These are the columns
///   archive queries filter on with equality (`WHERE app = 'web'`), and a
///   bloom filter lets the reader skip a row group that definitely holds
///   no matching rows. We deliberately do *not* put one on `line`: a bloom
///   filter answers "is value X present", which does nothing for a
///   substring `LIKE '%error%'`. Substring scans rely on columnar pruning
///   and min/max statistics instead.
fn log_writer_properties() -> WriterProperties {
    // 1% target false-positive rate (Parquet's default is 5%), sized for up
    // to ~10k distinct values — generous for app/namespace names, and small
    // enough that the filter itself costs almost nothing.
    const BLOOM_FPP: f64 = 0.01;
    const BLOOM_NDV: u64 = 10_000;

    let mut builder = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .set_max_row_group_row_count(Some(LOG_ROW_GROUP_SIZE));
    for column in ["app", "namespace"] {
        builder = builder
            .set_column_bloom_filter_enabled(column.into(), true)
            .set_column_bloom_filter_fpp(column.into(), BLOOM_FPP)
            .set_column_bloom_filter_max_ndv(column.into(), BLOOM_NDV);
    }
    builder.build()
}

/// Arrow schema for the logs table.
///
/// `instance` is nullable because the node's own lines (`bun` startup
/// messages) come from no instance.
pub fn log_schema() -> Schema {
    Schema::new(vec![
        Field::new("timestamp", DataType::UInt64, false),
        Field::new("app", DataType::Utf8, false),
        Field::new("namespace", DataType::Utf8, false),
        Field::new("stream", DataType::Utf8, false),
        Field::new("line", DataType::Utf8, false),
        Field::new("sequence", DataType::UInt64, false),
        Field::new("instance", DataType::Utf8, true),
    ])
}

/// Turn query batches into [`LogEntry`] rows, reading columns by name.
///
/// A query must select `timestamp`, `stream` and `line`. When it leaves out
/// `sequence`, rows are numbered in the order the query returned them, which
/// is the order its own `ORDER BY` asked for.
pub(crate) fn batches_to_entries(batches: &[RecordBatch]) -> Result<Vec<LogEntry>, KetchupError> {
    let mut entries = Vec::new();
    for batch in batches {
        let timestamps = required_column::<UInt64Array>(batch, "timestamp")?;
        let streams = required_column::<StringArray>(batch, "stream")?;
        let lines = required_column::<StringArray>(batch, "line")?;
        let sequences = optional_column::<UInt64Array>(batch, "sequence");
        let instances = optional_column::<StringArray>(batch, "instance");
        for row in 0..batch.num_rows() {
            let stream = match streams.value(row) {
                "stderr" => LogStream::Stderr,
                _ => LogStream::Stdout,
            };
            let sequence = match sequences {
                Some(column) => column.value(row),
                None => entries.len() as u64,
            };
            let instance = instances
                .filter(|column| column.is_valid(row))
                .map(|column| column.value(row).to_string());
            entries.push(LogEntry {
                timestamp: timestamps.value(row),
                sequence,
                instance,
                stream,
                line: lines.value(row).to_string(),
            });
        }
    }
    Ok(entries)
}

fn required_column<'a, T: 'static>(
    batch: &'a RecordBatch,
    name: &str,
) -> Result<&'a T, KetchupError> {
    batch
        .column_by_name(name)
        .and_then(|column| column.as_any().downcast_ref::<T>())
        .ok_or_else(|| {
            KetchupError::Io(std::io::Error::other(format!(
                "query result has no usable {name} column"
            )))
        })
}

fn optional_column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> Option<&'a T> {
    batch
        .column_by_name(name)
        .and_then(|column| column.as_any().downcast_ref::<T>())
}

/// Name of the ingest checkpoint kept beside the Parquet files.
const CHECKPOINT_FILE: &str = "ingest-checkpoint.json";

/// What the store has durably ingested, saved after every successful flush.
///
/// `offsets` is the highest capture-file offset whose line reached Parquet,
/// per capture file. A restarted agent re-reads capture files from the start;
/// the store skips every line at or below these offsets instead of storing it
/// a second time under a new timestamp. `last_sequence` keeps
/// [`LogEntry::sequence`] rising across a restart even if the clock stepped
/// back while the node was down.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct IngestCheckpoint {
    last_sequence: u64,
    offsets: std::collections::BTreeMap<PathBuf, u64>,
}

impl IngestCheckpoint {
    /// Load the checkpoint from `data_dir`, forgetting capture files that no
    /// longer exist. A missing or unreadable checkpoint starts empty: at worst
    /// the store ingests a line twice, never loses one.
    fn load(data_dir: &std::path::Path) -> Self {
        let path = data_dir.join(CHECKPOINT_FILE);
        let mut checkpoint = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                eprintln!(
                    "ketchup: ignoring unreadable ingest checkpoint {}: {error}",
                    path.display()
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        checkpoint.offsets.retain(|file, _| file.exists());
        checkpoint
    }

    /// Durably replace the checkpoint: a unique temp file, fsync, rename over
    /// the old one, then fsync the directory. A reader sees the old
    /// checkpoint or the new one, never half of either.
    fn save(&self, data_dir: &std::path::Path) -> Result<(), KetchupError> {
        let bytes = serde_json::to_vec(self)
            .map_err(|error| KetchupError::Io(std::io::Error::other(error.to_string())))?;
        crate::sesame::identity::atomic_write(&data_dir.join(CHECKPOINT_FILE), &bytes)?;
        Ok(())
    }
}

/// Returns the next flush counter for `data_dir`, one past the highest existing
/// `{prefix}_NNNNNN.parquet` file (or 0 if none), so restarts don't overwrite.
fn next_flush_counter(data_dir: &std::path::Path, prefix: &str) -> u64 {
    let mut max_seen: Option<u64> = None;
    if let Ok(entries) = std::fs::read_dir(data_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix(&format!("{prefix}_"))
                && let Some(digits) = rest.strip_suffix(".parquet")
                && let Ok(n) = digits.parse::<u64>()
            {
                max_seen = Some(max_seen.map_or(n, |m| m.max(n)));
            }
        }
    }
    max_seen.map_or(0, |m| m + 1)
}

/// Whether `data_dir` contains at least one `.parquet` file.
fn dir_has_parquet(data_dir: &std::path::Path) -> bool {
    std::fs::read_dir(data_dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "parquet"))
        })
        .unwrap_or(false)
}

/// A drained log buffer ready to be written to Parquet, decoupled from the
/// store so the caller can release its lock before the (blocking) write (M7).
pub struct LogPendingFlush {
    data_dir: std::path::PathBuf,
    path: std::path::PathBuf,
    batch: RecordBatch,
    /// What the store will have durably ingested once `batch` is on disk.
    checkpoint: IngestCheckpoint,
}

/// Persist a [`LogPendingFlush`] on the blocking pool, with no lock held so
/// concurrent appends/queries proceed while the write is in flight (M7).
/// Durable write (M6): temp file, fsync, atomic rename, dir fsync. The ingest
/// checkpoint follows the Parquet file, never precedes it, so a crash between
/// the two re-ingests one batch rather than losing it.
pub async fn write_log_pending(pending: LogPendingFlush) -> Result<(), KetchupError> {
    let LogPendingFlush {
        data_dir,
        path,
        batch,
        checkpoint,
    } = pending;
    tokio::task::spawn_blocking(move || -> Result<(), KetchupError> {
        std::fs::create_dir_all(&data_dir)?;
        let tmp = path.with_extension("parquet.tmp");
        let file = std::fs::File::create(&tmp)?;
        let mut writer =
            ArrowWriter::try_new(file, Arc::new(log_schema()), Some(log_writer_properties()))
                .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        writer
            .write(&batch)
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        let file = writer
            .into_inner()
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        if let Some(dir) = path.parent()
            && let Ok(dir_file) = std::fs::File::open(dir)
        {
            let _ = dir_file.sync_all();
        }
        checkpoint.save(&data_dir)
    })
    .await
    .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?
}

/// Flush a shared log store without holding its write lock across the (blocking)
/// encode + write (M7). Drains under a brief lock, releases it, then writes on
/// the blocking pool. Returns `true` if a file was written.
///
/// Extracted from the `bun` shutdown path (OBS7) so the "flush the shared
/// buffer on stop" step is unit-tested here instead of living only in the
/// binary.
pub async fn flush_shared(
    store: &std::sync::Arc<tokio::sync::RwLock<LogStore>>,
) -> Result<bool, KetchupError> {
    let pending = {
        let mut guard = store.write().await;
        guard.take_flush_batch()?
    };
    match pending {
        Some(p) => {
            write_log_pending(p).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// A buffered log entry waiting to be flushed.
struct BufferedLogEntry {
    timestamp: u64,
    sequence: u64,
    app: String,
    namespace: String,
    instance: Option<String>,
    stream: LogStream,
    line: String,
}

/// Arrow/DataFusion log store.
///
/// Same architecture as MayoStore: buffer in memory, flush to Parquet, query
/// via DataFusion SQL over the on-disk Parquet directory unioned with the
/// unflushed buffer. Persisted logs survive restarts and in-memory use stays
/// bounded to the buffer.
pub struct LogStore {
    buffer: Vec<BufferedLogEntry>,
    data_dir: PathBuf,
    /// Seeded past any existing `logs_NNNNNN.parquet` so restarts don't clobber.
    flush_counter: u64,
    /// Everything ingested so far, flushed or not. Flushing saves a copy.
    ingested: IngestCheckpoint,
}

impl LogStore {
    /// Open (or create) a log store writing Parquet to `data_dir`.
    ///
    /// Existing `logs_NNNNNN.parquet` files remain queryable, the flush
    /// counter resumes past the highest one, and the ingest checkpoint picks
    /// up where the last successful flush left it.
    pub fn new(data_dir: PathBuf) -> Self {
        let flush_counter = next_flush_counter(&data_dir, "logs");
        let ingested = IngestCheckpoint::load(&data_dir);
        Self {
            buffer: Vec::new(),
            data_dir,
            flush_counter,
            ingested,
        }
    }

    /// The directory where Parquet files are stored.
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Append a line the node itself wrote (not a workload's output).
    pub fn append(&mut self, app: &str, namespace: &str, stream: LogStream, line: &str) {
        let now = now_nanos();
        self.push(
            now / NANOS_PER_SECOND,
            now,
            app,
            namespace,
            None,
            stream,
            line,
        );
    }

    /// Append a log line with an explicit timestamp (for testing).
    pub fn append_at(
        &mut self,
        timestamp: u64,
        app: &str,
        namespace: &str,
        stream: LogStream,
        line: &str,
    ) {
        let nanos = timestamp.saturating_mul(NANOS_PER_SECOND);
        self.push(timestamp, nanos, app, namespace, None, stream, line);
    }

    /// Ingest a workload line from a log forwarder, stamped with the current
    /// time.
    ///
    /// Returns `false`, storing nothing, when the line's capture position is
    /// at or below what this store already holds from that file: a restarted
    /// agent re-reads capture files from the start, and those lines are
    /// already here under their original timestamps.
    pub fn ingest(&mut self, record: &super::types::LogRecord) -> bool {
        self.ingest_at_nanos(now_nanos(), record)
    }

    /// As [`ingest`](Self::ingest) with an explicit wall-clock time (for
    /// testing).
    pub fn ingest_at(&mut self, timestamp: u64, record: &super::types::LogRecord) -> bool {
        self.ingest_at_nanos(timestamp.saturating_mul(NANOS_PER_SECOND), record)
    }

    fn ingest_at_nanos(&mut self, nanos: u64, record: &super::types::LogRecord) -> bool {
        if let Some(position) = &record.position {
            let seen = self.ingested.offsets.get(&position.file).copied();
            if seen.is_some_and(|offset| position.end_offset <= offset) {
                return false;
            }
            self.ingested
                .offsets
                .insert(position.file.clone(), position.end_offset);
        }
        self.push(
            nanos / NANOS_PER_SECOND,
            nanos,
            &record.app,
            &record.namespace,
            Some(record.instance.clone()),
            record.stream,
            &record.line,
        );
        true
    }

    // One argument per stored column; a struct would only rename them.
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        timestamp: u64,
        nanos: u64,
        app: &str,
        namespace: &str,
        instance: Option<String>,
        stream: LogStream,
        line: &str,
    ) {
        // Wall-clock nanoseconds order lines across nodes; the bump keeps the
        // order strict within this node when two lines share a clock reading
        // or the clock steps back.
        let sequence = nanos.max(self.ingested.last_sequence.saturating_add(1));
        self.ingested.last_sequence = sequence;
        self.buffer.push(BufferedLogEntry {
            timestamp,
            sequence,
            app: app.to_string(),
            namespace: namespace.to_string(),
            instance,
            stream,
            line: line.to_string(),
        });
        // Bound memory if flushing is failing (M8): drop the oldest entries
        // rather than let a stuck flush grow the buffer until the node OOMs.
        if self.buffer.len() > MAX_BUFFER_ROWS {
            let overflow = self.buffer.len() - MAX_BUFFER_ROWS;
            self.buffer.drain(0..overflow);
        }
    }

    /// Number of unflushed entries.
    pub fn buffer_len(&self) -> usize {
        self.buffer.len()
    }

    /// Convert the buffer to an Arrow RecordBatch.
    fn buffer_to_batch(&self) -> Result<Option<RecordBatch>, KetchupError> {
        if self.buffer.is_empty() {
            return Ok(None);
        }

        let timestamps: Vec<u64> = self.buffer.iter().map(|e| e.timestamp).collect();
        let apps: Vec<&str> = self.buffer.iter().map(|e| e.app.as_str()).collect();
        let namespaces: Vec<&str> = self.buffer.iter().map(|e| e.namespace.as_str()).collect();
        let streams: Vec<&str> = self
            .buffer
            .iter()
            .map(|e| match e.stream {
                LogStream::Stdout => "stdout",
                LogStream::Stderr => "stderr",
            })
            .collect();
        let lines: Vec<&str> = self.buffer.iter().map(|e| e.line.as_str()).collect();
        let sequences: Vec<u64> = self.buffer.iter().map(|e| e.sequence).collect();
        let instances: Vec<Option<&str>> =
            self.buffer.iter().map(|e| e.instance.as_deref()).collect();

        let batch = RecordBatch::try_new(
            Arc::new(log_schema()),
            vec![
                Arc::new(UInt64Array::from(timestamps)),
                Arc::new(StringArray::from(apps)),
                Arc::new(StringArray::from(namespaces)),
                Arc::new(StringArray::from(streams)),
                Arc::new(StringArray::from(lines)),
                Arc::new(UInt64Array::from(sequences)),
                Arc::new(StringArray::from(instances)),
            ],
        )
        .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;

        Ok(Some(batch))
    }

    /// Drain the buffer into a self-contained [`LogPendingFlush`] the caller
    /// writes later, outside any lock (M7). Bumps the flush counter and clears
    /// the buffer immediately, so the file name is reserved before the (slow)
    /// write. Returns `None` when there's nothing to flush.
    pub fn take_flush_batch(&mut self) -> Result<Option<LogPendingFlush>, KetchupError> {
        let Some(batch) = self.buffer_to_batch()? else {
            return Ok(None);
        };
        let filename = format!("logs_{:06}.parquet", self.flush_counter);
        let path = self.data_dir.join(filename);
        self.buffer.clear();
        self.flush_counter += 1;
        Ok(Some(LogPendingFlush {
            data_dir: self.data_dir.clone(),
            path,
            batch,
            checkpoint: self.ingested.clone(),
        }))
    }

    /// Flush the buffer to Parquet.
    ///
    /// Convenience keeping the whole operation under `&mut self`; the encode +
    /// write runs on the blocking pool (M7). Callers holding a shared lock
    /// across readers should prefer [`take_flush_batch`](Self::take_flush_batch)
    /// + [`write_log_pending`] so the lock is released during the I/O.
    pub async fn flush(&mut self) -> Result<(), KetchupError> {
        let Some(pending) = self.take_flush_batch()? else {
            return Ok(());
        };
        write_log_pending(pending).await
    }

    /// Build a DataFusion session exposing a `logs` table over the on-disk
    /// Parquet directory unioned with the unflushed buffer.
    async fn session(&self) -> Result<SessionContext, KetchupError> {
        self.session_with_memory_limit(None).await
    }

    /// As [`session`](Self::session), optionally capping the query's working
    /// memory. A limit makes a runaway aggregation or sort *error* instead of
    /// exhausting the host (OBS5).
    async fn session_with_memory_limit(
        &self,
        memory_limit_bytes: Option<usize>,
    ) -> Result<SessionContext, KetchupError> {
        // Read Parquet string columns as `Utf8`, not `Utf8View`, so on-disk
        // batches share the canonical `log_schema` with the in-memory buffer.
        let config = SessionConfig::new().set_bool(
            "datafusion.execution.parquet.schema_force_view_types",
            false,
        );
        let ctx = if let Some(limit) = memory_limit_bytes {
            let runtime = datafusion::execution::runtime_env::RuntimeEnvBuilder::new()
                .with_memory_limit(limit, 1.0)
                .build_arc()
                .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
            SessionContext::new_with_config_rt(config, runtime)
        } else {
            SessionContext::new_with_config(config)
        };
        let schema = Arc::new(log_schema());

        // On-disk logs: a streaming `ListingTable` over the Parquet directory
        // (M19), so a large archive is scanned incrementally and charged to the
        // memory pool — the old code `.collect()`ed every Parquet row into a
        // MemTable before planning, so the OBS5 limit covered only aggregation,
        // not the dominant base scan. An empty directory registers an empty
        // table so the union view below always resolves.
        if dir_has_parquet(&self.data_dir) {
            let url = ListingTableUrl::parse(self.data_dir.to_string_lossy().as_ref())
                .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
            let options = ListingOptions::new(Arc::new(ParquetFormat::default()))
                .with_file_extension(".parquet");
            let listing = ListingTableConfig::new(url)
                .with_listing_options(options)
                .with_schema(schema.clone());
            let table = ListingTable::try_new(listing)
                .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
            ctx.register_table("logs_disk", Arc::new(table))
                .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        } else {
            let empty = MemTable::try_new(schema.clone(), vec![vec![]])
                .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
            ctx.register_table("logs_disk", Arc::new(empty))
                .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        }

        // Unflushed buffer: a small MemTable (bounded by the flush interval).
        let buffer_batch = self
            .buffer_to_batch()?
            .unwrap_or_else(|| RecordBatch::new_empty(schema.clone()));
        let buffer_table = MemTable::try_new(schema.clone(), vec![vec![buffer_batch]])
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        ctx.register_table("logs_buffer", Arc::new(buffer_table))
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;

        // Expose the union as `logs` so queries are unchanged.
        ctx.sql("CREATE VIEW logs AS SELECT * FROM logs_disk UNION ALL SELECT * FROM logs_buffer")
            .await
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        Ok(ctx)
    }

    /// Query logs using SQL under safety bounds (OBS5).
    ///
    /// The public `/v1/logs/sql` endpoint used to hand `?q=` straight to
    /// DataFusion with no guardrail. This method:
    ///
    /// - accepts only a read-only `SELECT`/`WITH` query (no `INSERT`,
    ///   `CREATE`, `DROP`, `COPY`, …), so the endpoint can't mutate anything;
    /// - runs against a session that registers only the `logs` table, so a
    ///   reference to any other table fails to plan rather than reading it;
    /// - caps the rows returned to [`MAX_LOG_SQL_ROWS`] by wrapping the query
    ///   in an outer `LIMIT`; and
    /// - runs under a [`LOG_SQL_MEMORY_LIMIT_BYTES`] working-memory limit, so a
    ///   runaway aggregation errors rather than exhausting the host.
    pub async fn query_sql_json_bounded(
        &self,
        sql: &str,
    ) -> Result<Vec<serde_json::Value>, KetchupError> {
        let ctx = self
            .session_with_memory_limit(Some(LOG_SQL_MEMORY_LIMIT_BYTES))
            .await?;

        // Reject non-read statements up front, before planning, so an error
        // message names the problem clearly.
        let trimmed = sql.trim_start();
        let head = trimmed
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if head != "SELECT" && head != "WITH" {
            return Err(KetchupError::QueryRejected {
                reason: "only read-only SELECT/WITH queries are allowed".to_string(),
            });
        }

        // `logs` is the only table this session registers, so any reference to
        // another table fails to plan below — a query can't escape the log
        // table to read other data. Cap the result set by wrapping the query
        // in an outer LIMIT, which bounds rows regardless of the inner query.
        let bounded = format!("SELECT * FROM ({trimmed}) AS bounded LIMIT {MAX_LOG_SQL_ROWS}");
        let df = ctx
            .sql(&bounded)
            .await
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        let batches = df
            .collect()
            .await
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;

        Ok(batches_to_json(&batches))
    }

    /// Query logs using SQL, returning structured LogEntry results.
    ///
    /// The query must select at least `timestamp`, `stream` and `line`; see
    /// [`batches_to_entries`] for how the optional columns are read.
    pub async fn query_sql(&self, sql: &str) -> Result<Vec<LogEntry>, KetchupError> {
        let ctx = self.session().await?;
        let df = ctx
            .sql(sql)
            .await
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;

        let batches = df
            .collect()
            .await
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;

        batches_to_entries(&batches)
    }

    /// Query one app's logs by time range and grep pattern, oldest first.
    ///
    /// Rows come back in ingest order (`sequence`), which is emission order
    /// per instance. With `tail`, only the newest `tail` matching rows come
    /// back, still oldest first.
    pub async fn query(
        &self,
        app: &str,
        namespace: &str,
        start: Option<u64>,
        end: Option<u64>,
        grep: Option<&str>,
        tail: Option<usize>,
    ) -> Result<Vec<LogEntry>, KetchupError> {
        // M1: escape single quotes so an app/namespace/grep param can't
        // break out of the SQL string literal and read other tenants' logs.
        let app = escape_sql_literal(app);
        let namespace = escape_sql_literal(namespace);
        let mut conditions = vec![
            format!("app = '{app}'"),
            format!("namespace = '{namespace}'"),
        ];
        if let Some(s) = start {
            conditions.push(format!("timestamp >= {s}"));
        }
        if let Some(e) = end {
            conditions.push(format!("timestamp <= {e}"));
        }
        if let Some(g) = grep {
            let g = escape_sql_literal(g);
            conditions.push(format!("line LIKE '%{g}%'"));
        }

        let where_clause = conditions.join(" AND ");
        let select = format!(
            "SELECT timestamp, app, namespace, stream, line, sequence, instance \
             FROM logs WHERE {where_clause}"
        );
        // A tail takes the newest rows, then puts them back in order.
        let sql = match tail {
            Some(tail) => format!(
                "SELECT * FROM ({select} ORDER BY sequence DESC LIMIT {tail}) AS tailed \
                 ORDER BY sequence"
            ),
            None => format!("{select} ORDER BY sequence"),
        };
        self.query_sql(&sql).await
    }
}

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Wall-clock nanoseconds since the Unix epoch, saturating far in the future.
fn now_nanos() -> u64 {
    let since_epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since_epoch.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (LogStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = LogStore::new(dir.path().to_path_buf());
        (store, dir)
    }

    /// OBS7: a clean shutdown must flush whatever the buffer still holds, so
    /// the last lines survive a restart instead of being dropped.
    #[tokio::test]
    async fn flush_shared_persists_the_buffer_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(tokio::sync::RwLock::new(LogStore::new(
            dir.path().to_path_buf(),
        )));
        store.write().await.append_at(
            1,
            "web",
            "default",
            LogStream::Stdout,
            "last line before stop",
        );

        assert!(flush_shared(&store).await.unwrap(), "buffer should flush");
        assert_eq!(store.read().await.buffer_len(), 0);

        // A fresh store over the same dir sees the flushed line.
        let reopened = LogStore::new(dir.path().to_path_buf());
        let results = reopened
            .query("web", "default", None, None, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line, "last line before stop");
    }

    #[tokio::test]
    async fn flush_shared_reports_empty_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(tokio::sync::RwLock::new(LogStore::new(
            dir.path().to_path_buf(),
        )));
        assert!(!flush_shared(&store).await.unwrap(), "nothing to flush");
    }

    #[tokio::test]
    async fn append_and_query_without_flush() {
        let (mut store, _dir) = test_store();
        store.append_at(1000, "web", "default", LogStream::Stdout, "hello world");

        let results = store
            .query("web", "default", None, None, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line, "hello world");
        assert_eq!(results[0].timestamp, 1000);
    }

    // --- OBS5: bounded raw-log SQL ----------------------------------------

    #[tokio::test]
    async fn bounded_sql_runs_a_plain_select() {
        let (mut store, _dir) = test_store();
        store.append_at(1, "web", "default", LogStream::Stdout, "hello");
        let rows = store
            .query_sql_json_bounded("SELECT line FROM logs")
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["line"], "hello");
    }

    #[tokio::test]
    async fn bounded_sql_rejects_non_select() {
        let (store, _dir) = test_store();
        for sql in [
            "DROP TABLE logs",
            "INSERT INTO logs VALUES (1,'a','b','stdout','x')",
            "CREATE TABLE evil AS SELECT * FROM logs",
        ] {
            let err = store.query_sql_json_bounded(sql).await.unwrap_err();
            assert!(
                matches!(err, KetchupError::QueryRejected { .. }),
                "{sql} was not rejected: {err:?}"
            );
        }
    }

    /// A query naming any table other than `logs` must fail — whether the
    /// bounded validator rejects it, or DataFusion refuses to plan an
    /// unregistered table. Either way it must NOT return that table's data.
    #[tokio::test]
    async fn bounded_sql_rejects_other_tables() {
        let (store, _dir) = test_store();
        for sql in [
            "SELECT * FROM information_schema.tables",
            "SELECT * FROM secrets",
        ] {
            assert!(
                store.query_sql_json_bounded(sql).await.is_err(),
                "{sql} was not rejected"
            );
        }
    }

    #[tokio::test]
    async fn bounded_sql_runs_a_join_and_aggregate_over_logs_only() {
        let (mut store, _dir) = test_store();
        store.append_at(1, "web", "default", LogStream::Stdout, "a");
        store.append_at(2, "web", "default", LogStream::Stdout, "b");
        // A self-referential query over `logs` (a CTE) still plans and runs.
        let rows = store
            .query_sql_json_bounded(
                "WITH c AS (SELECT app, COUNT(*) n FROM logs GROUP BY app) SELECT app, n FROM c",
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["n"], 2);
    }

    #[tokio::test]
    async fn bounded_sql_caps_returned_rows() {
        let (mut store, _dir) = test_store();
        // More rows than the cap; an unbounded SELECT would return them all.
        for i in 0..(MAX_LOG_SQL_ROWS as u64 + 500) {
            store.append_at(i, "web", "default", LogStream::Stdout, "row");
        }
        let rows = store
            .query_sql_json_bounded("SELECT timestamp FROM logs")
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            MAX_LOG_SQL_ROWS,
            "row cap not enforced (got {} rows)",
            rows.len()
        );
    }

    #[test]
    fn escape_sql_literal_doubles_quotes() {
        assert_eq!(escape_sql_literal("web"), "web");
        assert_eq!(escape_sql_literal("a' OR '1'='1"), "a'' OR ''1''=''1");
    }

    /// M1: an injection payload in the app filter must not read another
    /// tenant's logs.
    #[tokio::test]
    async fn query_app_injection_is_neutralised() {
        let (mut store, _dir) = test_store();
        store.append_at(1, "secret", "prod", LogStream::Stdout, "top secret");

        let results = store
            .query("x' OR '1'='1", "default", None, None, None, None)
            .await
            .unwrap();
        assert!(results.is_empty(), "SQL injection leaked logs: {results:?}");
    }

    #[tokio::test]
    async fn reopen_reads_persisted_logs_without_clobbering() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = LogStore::new(dir.path().to_path_buf());
            store.append_at(1, "web", "default", LogStream::Stdout, "first");
            store.flush().await.unwrap();
            store.append_at(2, "web", "default", LogStream::Stdout, "second");
            store.flush().await.unwrap();
        }

        // Reopen over the same dir: prior logs are queryable, files untouched.
        let mut store = LogStore::new(dir.path().to_path_buf());
        let results = store
            .query("web", "default", None, None, None, None)
            .await
            .unwrap();
        assert_eq!(
            results.len(),
            2,
            "persisted logs not reloaded after restart"
        );

        store.append_at(3, "web", "default", LogStream::Stdout, "third");
        store.flush().await.unwrap();
        let files = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "parquet")
            })
            .count();
        assert_eq!(files, 3, "restart clobbered an existing log file");
    }

    /// V02 soak regression: `relish logs --tail N` must return the newest N
    /// lines, oldest first, even when they all share one second and straddle
    /// several Parquet files and the unflushed buffer.
    #[tokio::test]
    async fn tail_returns_the_newest_lines_in_emission_order() {
        let (mut store, _dir) = test_store();
        for i in 0..30 {
            store.append_at(
                7,
                "writer",
                "default",
                LogStream::Stdout,
                &format!("ACK {i}"),
            );
            if i % 8 == 7 {
                store.flush().await.unwrap();
            }
        }

        let lines: Vec<String> = store
            .query("writer", "default", None, None, None, Some(10))
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.line)
            .collect();
        let expected: Vec<String> = (20..30).map(|i| format!("ACK {i}")).collect();
        assert_eq!(lines, expected);
    }

    /// A writer logging ten lines a second puts ten rows under every
    /// one-second timestamp. Sorting on the timestamp alone left their order
    /// to DataFusion, which interleaved Parquet files (`ACK 968, 974, 969`).
    #[tokio::test]
    async fn lines_within_one_second_keep_emission_order_across_flushes() {
        let (mut store, _dir) = test_store();
        for i in 0..200 {
            store.append_at(
                7,
                "writer",
                "default",
                LogStream::Stdout,
                &format!("ACK {i}"),
            );
            if i % 9 == 8 {
                store.flush().await.unwrap();
            }
        }

        let lines: Vec<String> = store
            .query("writer", "default", None, None, None, None)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.line)
            .collect();
        let expected: Vec<String> = (0..200).map(|i| format!("ACK {i}")).collect();
        assert_eq!(lines, expected);
    }

    /// A line the writer app printed, as the forwarder hands it over: `ACK n`
    /// is the n-th line of `file`, and every line is `LINE_BYTES` long.
    fn writer_line(file: &std::path::Path, n: u64) -> crate::ketchup::types::LogRecord {
        crate::ketchup::types::LogRecord {
            app: "writer".to_string(),
            namespace: "default".to_string(),
            instance: "writer-0".to_string(),
            stream: LogStream::Stdout,
            line: format!("ACK {n:04}"),
            position: Some(crate::ketchup::types::CapturePosition {
                file: file.to_path_buf(),
                end_offset: n * LINE_BYTES,
            }),
        }
    }

    const LINE_BYTES: u64 = "ACK 0000\n".len() as u64;

    /// A capture file that exists, so the checkpoint doesn't forget it.
    fn capture_file(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, b"").unwrap();
        path
    }

    async fn writer_lines(store: &LogStore) -> Vec<String> {
        store
            .query("writer", "default", None, None, None, None)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.line)
            .collect()
    }

    fn acks(range: std::ops::RangeInclusive<u64>) -> Vec<String> {
        range.map(|n| format!("ACK {n:04}")).collect()
    }

    /// V02 soak regression: after a SIGKILLed or powered-off Bun came back,
    /// the forwarder re-read every adopted container's capture file from the
    /// start and the store took all of it again under new timestamps, so
    /// hour-old lines showed up as the newest.
    #[tokio::test]
    async fn restart_does_not_reingest_lines_already_flushed() {
        let dir = tempfile::tempdir().unwrap();
        let captures = tempfile::tempdir().unwrap();
        let file = capture_file(&captures, "writer.stdout");
        {
            let mut store = LogStore::new(dir.path().to_path_buf());
            for n in 1..=10 {
                assert!(store.ingest_at(100, &writer_line(&file, n)));
            }
            store.flush().await.unwrap();
        }

        // The restarted agent replays the whole file, then new output follows.
        let mut store = LogStore::new(dir.path().to_path_buf());
        for n in 1..=10 {
            assert!(
                !store.ingest_at(4_000, &writer_line(&file, n)),
                "line {n} was ingested twice"
            );
        }
        for n in 11..=12 {
            assert!(store.ingest_at(4_000, &writer_line(&file, n)));
        }

        assert_eq!(writer_lines(&store).await, acks(1..=12));
        let tail = store
            .query("writer", "default", None, None, None, Some(1))
            .await
            .unwrap();
        assert_eq!(tail[0].line, "ACK 0012");
    }

    /// Lines still in the buffer when the node died never reached Parquet,
    /// so the replay after the restart must store them rather than skip them.
    #[tokio::test]
    async fn lines_lost_with_the_buffer_are_ingested_again_after_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let captures = tempfile::tempdir().unwrap();
        let file = capture_file(&captures, "writer.stdout");
        {
            let mut store = LogStore::new(dir.path().to_path_buf());
            for n in 1..=5 {
                store.ingest_at(100, &writer_line(&file, n));
            }
            store.flush().await.unwrap();
            for n in 6..=8 {
                store.ingest_at(100, &writer_line(&file, n));
            }
            // Power cut: dropped without a flush.
        }

        let mut store = LogStore::new(dir.path().to_path_buf());
        let stored: Vec<bool> = (1..=8)
            .map(|n| store.ingest_at(200, &writer_line(&file, n)))
            .collect();
        assert_eq!(
            stored,
            vec![false, false, false, false, false, true, true, true]
        );
        assert_eq!(writer_lines(&store).await, acks(1..=8));
    }

    /// A restarted instance writes a new capture file (a new generation), so
    /// the old file's offsets say nothing about it.
    #[tokio::test]
    async fn a_new_capture_file_starts_from_its_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let captures = tempfile::tempdir().unwrap();
        let old = capture_file(&captures, "generation-1.stdout");
        let new = capture_file(&captures, "generation-2.stdout");
        let mut store = LogStore::new(dir.path().to_path_buf());
        for n in 1..=3 {
            store.ingest_at(100, &writer_line(&old, n));
        }
        store.flush().await.unwrap();

        let mut store = LogStore::new(dir.path().to_path_buf());
        assert!(store.ingest_at(200, &writer_line(&new, 1)));
    }

    /// Sequence order must survive a restart even if the clock stepped back
    /// while the node was down, or the newest lines would sort first.
    #[tokio::test]
    async fn sequence_keeps_rising_across_a_restart_when_the_clock_steps_back() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = LogStore::new(dir.path().to_path_buf());
            store.append_at(1_000, "writer", "default", LogStream::Stdout, "ACK 0001");
            store.flush().await.unwrap();
        }
        let mut store = LogStore::new(dir.path().to_path_buf());
        store.append_at(5, "writer", "default", LogStream::Stdout, "ACK 0002");

        assert_eq!(writer_lines(&store).await, acks(1..=2));
    }

    /// A torn or hand-edited checkpoint must not stop the node logging; the
    /// worst case is ingesting some lines a second time.
    #[tokio::test]
    async fn unreadable_checkpoint_opens_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let captures = tempfile::tempdir().unwrap();
        let file = capture_file(&captures, "writer.stdout");
        std::fs::write(dir.path().join(CHECKPOINT_FILE), b"{\"last_seq").unwrap();

        let mut store = LogStore::new(dir.path().to_path_buf());
        assert!(store.ingest_at(100, &writer_line(&file, 1)));
        assert_eq!(writer_lines(&store).await, acks(1..=1));
    }

    /// Each flush replaces the checkpoint wholesale through a temp file and a
    /// rename, so no temp file is left behind and the file on disk is always
    /// one complete checkpoint.
    #[tokio::test]
    async fn flush_replaces_the_checkpoint_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let captures = tempfile::tempdir().unwrap();
        let file = capture_file(&captures, "writer.stdout");
        let mut store = LogStore::new(dir.path().to_path_buf());
        for n in 1..=2 {
            store.ingest_at(100, &writer_line(&file, n));
            store.flush().await.unwrap();
        }

        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !name.ends_with(".parquet"))
            .collect();
        assert_eq!(names, vec![CHECKPOINT_FILE.to_string()]);
        let saved: IngestCheckpoint =
            serde_json::from_slice(&std::fs::read(dir.path().join(CHECKPOINT_FILE)).unwrap())
                .unwrap();
        assert_eq!(saved.offsets.get(&file), Some(&(2 * LINE_BYTES)));
        assert_eq!(saved, store.ingested);
    }

    #[tokio::test]
    async fn ingested_entries_keep_their_instance_and_stream() {
        let (mut store, _dir) = test_store();
        let mut record = writer_line(std::path::Path::new("/nonexistent/writer.stderr"), 1);
        record.stream = LogStream::Stderr;
        store.ingest_at(100, &record);
        store.flush().await.unwrap();
        store.append_at(101, "writer", "default", LogStream::Stdout, "from the node");

        let entries = store
            .query("writer", "default", None, None, None, None)
            .await
            .unwrap();
        assert_eq!(entries[0].instance.as_deref(), Some("writer-0"));
        assert_eq!(entries[0].stream, LogStream::Stderr);
        assert_eq!(entries[0].timestamp, 100);
        assert_eq!(entries[1].instance, None);
        assert!(entries[0].sequence < entries[1].sequence);
    }

    #[tokio::test]
    async fn query_after_flush() {
        let (mut store, _dir) = test_store();
        store.append_at(1000, "web", "default", LogStream::Stdout, "flushed line");
        store.flush().await.unwrap();

        let results = store
            .query("web", "default", None, None, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line, "flushed line");
    }

    #[tokio::test]
    async fn query_sees_flushed_and_unflushed() {
        let (mut store, _dir) = test_store();
        store.append_at(1, "web", "default", LogStream::Stdout, "old");
        store.flush().await.unwrap();
        store.append_at(2, "web", "default", LogStream::Stdout, "new");

        let results = store
            .query("web", "default", None, None, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].line, "old");
        assert_eq!(results[1].line, "new");
    }

    #[tokio::test]
    async fn query_filters_by_app() {
        let (mut store, _dir) = test_store();
        store.append_at(1, "web", "default", LogStream::Stdout, "web log");
        store.append_at(1, "api", "default", LogStream::Stdout, "api log");

        let results = store
            .query("web", "default", None, None, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line, "web log");
    }

    #[tokio::test]
    async fn query_filters_by_time_range() {
        let (mut store, _dir) = test_store();
        store.append_at(100, "web", "default", LogStream::Stdout, "early");
        store.append_at(200, "web", "default", LogStream::Stdout, "middle");
        store.append_at(300, "web", "default", LogStream::Stdout, "late");

        let results = store
            .query("web", "default", Some(150), Some(250), None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line, "middle");
    }

    #[tokio::test]
    async fn query_with_grep() {
        let (mut store, _dir) = test_store();
        store.append_at(1, "web", "default", LogStream::Stdout, "INFO starting");
        store.append_at(2, "web", "default", LogStream::Stderr, "ERROR failed");
        store.append_at(3, "web", "default", LogStream::Stdout, "INFO ready");

        let results = store
            .query("web", "default", None, None, Some("ERROR"), None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].line.contains("ERROR"));
        assert_eq!(results[0].stream, LogStream::Stderr);
    }

    #[tokio::test]
    async fn query_with_limit() {
        let (mut store, _dir) = test_store();
        for i in 0..10 {
            store.append_at(i, "web", "default", LogStream::Stdout, &format!("line {i}"));
        }

        let results = store
            .query("web", "default", None, None, None, Some(3))
            .await
            .unwrap();
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn query_empty_store() {
        let (store, _dir) = test_store();
        let results = store
            .query("web", "default", None, None, None, None)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn flush_creates_parquet() {
        let (mut store, dir) = test_store();
        store.append_at(1, "web", "default", LogStream::Stdout, "test");
        store.flush().await.unwrap();

        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "parquet"))
            .collect();
        assert_eq!(files.len(), 1);
    }

    #[tokio::test]
    async fn flush_clears_buffer() {
        let (mut store, _dir) = test_store();
        store.append_at(1, "web", "default", LogStream::Stdout, "test");
        assert_eq!(store.buffer_len(), 1);
        store.flush().await.unwrap();
        assert_eq!(store.buffer_len(), 0);
    }

    #[tokio::test]
    async fn multiple_apps_filtered() {
        let (mut store, _dir) = test_store();
        store.append_at(1, "web", "prod", LogStream::Stdout, "web prod");
        store.append_at(1, "api", "prod", LogStream::Stdout, "api prod");
        store.append_at(1, "web", "staging", LogStream::Stdout, "web staging");

        let results = store
            .query("web", "prod", None, None, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line, "web prod");
    }

    #[tokio::test]
    async fn schema_has_seven_columns() {
        let schema = log_schema();
        assert_eq!(schema.fields().len(), 7);
        assert_eq!(schema.field(0).name(), "timestamp");
        assert_eq!(schema.field(1).name(), "app");
        assert_eq!(schema.field(2).name(), "namespace");
        assert_eq!(schema.field(3).name(), "stream");
        assert_eq!(schema.field(4).name(), "line");
        assert_eq!(schema.field(5).name(), "sequence");
        assert_eq!(schema.field(6).name(), "instance");
    }

    // --- Phase 12: ZSTD compression + bloom filters (archive path) ---

    use datafusion::parquet::file::properties::ReaderProperties;
    use datafusion::parquet::file::reader::{FileReader, SerializedFileReader};
    use datafusion::parquet::file::serialized_reader::ReadOptionsBuilder;

    /// Write one Parquet file from `batch` with `props`; return (tempdir, path).
    fn write_parquet(
        batch: &RecordBatch,
        props: WriterProperties,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs_000000.parquet");
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = ArrowWriter::try_new(file, Arc::new(log_schema()), Some(props)).unwrap();
        writer.write(batch).unwrap();
        writer.close().unwrap();
        (dir, path)
    }

    /// A log batch with the given `app` and `line` columns and realistic
    /// values everywhere else: one-second timestamps, nanosecond sequences
    /// roughly 10 ms apart, and a handful of instances.
    fn batch_of(apps: Vec<&str>, lines: Vec<&str>) -> RecordBatch {
        let rows = lines.len();
        let base = 1_790_368_624_000_000_000u64;
        let sequences: Vec<u64> = (0..rows as u64).map(|i| base + i * 10_000_371).collect();
        let timestamps: Vec<u64> = sequences.iter().map(|s| s / NANOS_PER_SECOND).collect();
        let instances: Vec<Option<&str>> = (0..rows)
            .map(|i| Some(["web-0", "web-1", "web-2"][i % 3]))
            .collect();
        RecordBatch::try_new(
            Arc::new(log_schema()),
            vec![
                Arc::new(UInt64Array::from(timestamps)),
                Arc::new(StringArray::from(apps)),
                Arc::new(StringArray::from(vec!["default"; rows])),
                Arc::new(StringArray::from(vec!["stdout"; rows])),
                Arc::new(StringArray::from(lines)),
                Arc::new(UInt64Array::from(sequences)),
                Arc::new(StringArray::from(instances)),
            ],
        )
        .unwrap()
    }

    /// A batch of semi-realistic, semi-repetitive log lines.
    fn log_batch(rows: usize) -> (RecordBatch, usize) {
        let apps: Vec<&str> = (0..rows)
            .map(|i| if i % 2 == 0 { "web" } else { "api" })
            .collect();
        let lines: Vec<String> = (0..rows)
            .map(|i| format!("GET /api/v1/users/{} 200 OK in {}ms", i % 100, i % 50))
            .collect();
        // Raw-text size: roughly what the flat .log file would hold.
        let raw_text_bytes: usize = lines
            .iter()
            .enumerate()
            .map(|(i, l)| l.len() + i.to_string().len() + 3) // "{ts} O {line}\n"
            .sum();
        let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        (batch_of(apps, line_refs), raw_text_bytes)
    }

    #[test]
    fn zstd_parquet_is_over_5x_smaller_than_raw_text() {
        let (batch, raw_text_bytes) = log_batch(20_000);
        let (_dir, path) = write_parquet(&batch, log_writer_properties());
        let compressed = std::fs::metadata(&path).unwrap().len() as usize;
        assert!(
            raw_text_bytes > compressed * 5,
            "expected >5x vs raw text: raw={raw_text_bytes} compressed={compressed}"
        );
    }

    #[tokio::test]
    async fn zstd_archive_round_trips_through_remote_query() {
        let (mut store, dir) = test_store();
        for i in 0..1000 {
            store.append_at(i, "web", "default", LogStream::Stdout, "round trip line");
        }
        store.flush().await.unwrap();

        let rows = crate::ketchup::remote_query::query_remote(
            dir.path().to_str().unwrap(),
            "SELECT timestamp, app, namespace, stream, line FROM logs ORDER BY timestamp",
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1000);
        assert_eq!(rows[0].line, "round trip line");
        assert_eq!(rows[999].timestamp, 999);
    }

    #[tokio::test]
    async fn bloom_filters_written_on_app_and_namespace_only() {
        let (mut store, dir) = test_store();
        store.append_at(1, "web", "default", LogStream::Stdout, "x");
        store.flush().await.unwrap();

        let path = dir.path().join("logs_000000.parquet");
        let reader = SerializedFileReader::new(std::fs::File::open(&path).unwrap()).unwrap();
        let rg = reader.metadata().row_group(0);
        // columns: 0=timestamp 1=app 2=namespace 3=stream 4=line
        assert!(
            rg.column(1).bloom_filter_offset().is_some(),
            "app needs a bloom filter"
        );
        assert!(
            rg.column(2).bloom_filter_offset().is_some(),
            "namespace needs a bloom filter"
        );
        assert!(
            rg.column(4).bloom_filter_offset().is_none(),
            "line must NOT have one"
        );
        assert!(
            rg.column(0).bloom_filter_offset().is_none(),
            "timestamp must NOT have one"
        );
    }

    #[tokio::test]
    async fn equality_query_on_archive_returns_correct_app() {
        let (mut store, dir) = test_store();
        store.append_at(1, "web", "default", LogStream::Stdout, "web line");
        store.append_at(2, "api", "default", LogStream::Stdout, "api line");
        store.flush().await.unwrap();

        let rows = crate::ketchup::remote_query::query_remote(
            dir.path().to_str().unwrap(),
            "SELECT timestamp, app, namespace, stream, line FROM logs WHERE app = 'web'",
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].line, "web line");
    }

    #[tokio::test]
    async fn time_range_random_access_across_row_groups() {
        // More than LOG_ROW_GROUP_SIZE rows, so the file spans several row
        // groups; a time-range query must still read just the right slice.
        let (mut store, dir) = test_store();
        let total = (LOG_ROW_GROUP_SIZE as u64) * 3;
        for i in 0..total {
            store.append_at(i, "web", "default", LogStream::Stdout, "line");
        }
        store.flush().await.unwrap();

        let rows = crate::ketchup::remote_query::query_remote(
            dir.path().to_str().unwrap(),
            "SELECT timestamp, app, namespace, stream, line FROM logs \
             WHERE timestamp >= 10000 AND timestamp < 10010",
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 10);
    }

    #[test]
    fn bloom_filter_false_positive_rate_under_one_percent() {
        // Write 2000 distinct app values, then probe 10k absent values and
        // measure the observed false-positive rate against our 1% target.
        let n = 2000usize;
        let apps: Vec<String> = (0..n).map(|i| format!("app-{i}")).collect();
        let app_refs: Vec<&str> = apps.iter().map(|s| s.as_str()).collect();
        let batch = batch_of(app_refs, vec!["x"; n]);
        let (_dir, path) = write_parquet(&batch, log_writer_properties());

        let props = ReaderProperties::builder()
            .set_read_bloom_filter(true)
            .build();
        let opts = ReadOptionsBuilder::new()
            .with_reader_properties(props)
            .build();
        let reader =
            SerializedFileReader::new_with_options(std::fs::File::open(&path).unwrap(), opts)
                .unwrap();
        let rg = reader.get_row_group(0).unwrap();
        let sbbf = rg
            .get_column_bloom_filter(1)
            .expect("app bloom filter present");

        // Bloom filters never report a false negative: present values must hit.
        for a in &apps[..100] {
            assert!(sbbf.check(&a.as_str()), "present value {a} not found");
        }

        let probes = 10_000usize;
        let fp = (0..probes)
            .filter(|i| {
                let absent = format!("absent-{i}");
                sbbf.check(&absent.as_str())
            })
            .count();
        let rate = fp as f64 / probes as f64;
        assert!(
            rate < 0.01,
            "false-positive rate {rate} exceeds 1% ({fp}/{probes})"
        );
    }
}
