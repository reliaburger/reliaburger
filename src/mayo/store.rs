//! Arrow/DataFusion-based time-series store.
//!
//! Metrics are buffered in memory, converted to Arrow RecordBatches,
//! and queryable via DataFusion SQL. Periodically flushed to Parquet
//! files for persistence. The same architecture as InfluxDB IOx.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use datafusion::arrow::array::{Array, Float64Array, StringArray, UInt64Array};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::prelude::*;
use object_store::ObjectStore;
use sha2::{Digest, Sha256};

use super::scan::{ParquetTable, list_local, list_remote, streaming_session};
use super::types::{MayoError, MetricKey, Sample};

/// Where a [`MayoStore`] reads and writes Parquet.
///
/// `Local` is the default single-node case: a filesystem directory. `Remote`
/// (H8) backs the store with an `object_store` bucket named by
/// `[metrics] object_store_url` (`s3://…`, `gs://…`, or `file://…`), so metrics
/// survive node loss and the same DataFusion queries run over the bucket.
#[derive(Clone)]
enum Backend {
    Local,
    Remote {
        store: Arc<dyn ObjectStore>,
        /// Path *inside* the store where `metrics_*.parquet` files live.
        prefix: object_store::path::Path,
    },
}

/// Parse a `[metrics] object_store_url` into an object store and key prefix.
/// A bare path or `file://…` maps to the local filesystem; `s3://…`/`gs://…`
/// map to their cloud backends (credentials from each backend's standard
/// environment variables). Mirrors Ketchup's log export.
fn parse_object_store(
    destination: &str,
) -> Result<(Arc<dyn ObjectStore>, object_store::path::Path), MayoError> {
    let url = if destination.contains("://") {
        url::Url::parse(destination)
            .map_err(|e| MayoError::ObjectStore(format!("invalid object_store_url: {e}")))?
    } else {
        let absolute = std::path::absolute(destination).map_err(MayoError::Io)?;
        url::Url::from_file_path(&absolute)
            .map_err(|_| MayoError::ObjectStore("could not build file:// url".to_string()))?
    };
    let (store, prefix) = crate::object_storage::open(&url)
        .map_err(|e| MayoError::ObjectStore(format!("unsupported object_store_url: {e}")))?;
    Ok((Arc::from(store), prefix))
}

/// Serialise a RecordBatch to in-memory Parquet bytes (for object-store PUT).
fn batch_to_parquet_bytes(batch: &RecordBatch) -> Result<Vec<u8>, MayoError> {
    let mut buffer = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut buffer, batch.schema(), None)
        .map_err(|e| MayoError::Arrow(e.to_string()))?;
    writer
        .write(batch)
        .map_err(|e| MayoError::Arrow(e.to_string()))?;
    writer
        .close()
        .map_err(|e| MayoError::Arrow(e.to_string()))?;
    Ok(buffer)
}

/// Most rows one per-app query returns from one node.
pub const APP_QUERY_ROW_LIMIT: usize = 10_000;

/// Most rows one unfiltered `/v1/metrics?name=*` query returns from one node.
pub const ALL_QUERY_ROW_LIMIT: usize = 10_000;

/// Escape a value for safe interpolation into a single-quoted SQL string
/// literal (M1). DataFusion follows standard SQL: a `'` inside a literal is
/// doubled. Without this, a query param like `x' OR '1'='1` breaks out of
/// the literal and can read other namespaces' data.
pub(crate) fn escape_sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}

/// Arrow schema for the metrics table.
pub fn metrics_schema() -> Schema {
    Schema::new(vec![
        Field::new("timestamp", DataType::UInt64, false),
        Field::new("metric_name", DataType::Utf8, false),
        Field::new("labels", DataType::Utf8, false),
        Field::new("value", DataType::Float64, false),
    ])
}

/// Returns the next flush counter for `data_dir`, one past the highest existing
/// `{prefix}_NNNNNN.parquet` file (or 0 if none). Used so a restart resumes
/// numbering instead of overwriting a previous run's files.
pub(crate) fn next_flush_counter(data_dir: &std::path::Path, prefix: &str) -> u64 {
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

/// Write a single RecordBatch to a Parquet file at `path`, durably (M6).
///
/// Writes to a `.tmp` sibling, fsyncs it, atomically renames it into place, and
/// fsyncs the directory — so a crash mid-write can't leave a torn file that a
/// later query treats as valid, and a flush that returned Ok is really on disk.
/// Synchronous (Arrow's writer is blocking), so callers run it on
/// `spawn_blocking` to keep the async runtime free (OBS5/M3).
pub(crate) fn write_batch_parquet(
    path: &std::path::Path,
    batch: &RecordBatch,
) -> Result<(), MayoError> {
    let tmp = path.with_extension("parquet.tmp");
    let file = std::fs::File::create(&tmp).map_err(MayoError::Io)?;
    let mut writer = ArrowWriter::try_new(file, batch.schema(), None)
        .map_err(|e| MayoError::Arrow(e.to_string()))?;
    writer
        .write(batch)
        .map_err(|e| MayoError::Arrow(e.to_string()))?;
    // `into_inner` flushes and hands back the File so we can fsync the bytes
    // before the rename publishes them.
    let file = writer
        .into_inner()
        .map_err(|e| MayoError::Arrow(e.to_string()))?;
    file.sync_all().map_err(MayoError::Io)?;
    std::fs::rename(&tmp, path).map_err(MayoError::Io)?;
    if let Some(dir) = path.parent() {
        // A dir fsync makes the rename itself durable. Best-effort: not every
        // filesystem supports it, and the file bytes are already synced.
        if let Ok(dir_file) = std::fs::File::open(dir) {
            let _ = dir_file.sync_all();
        }
    }
    Ok(())
}

/// A drained buffer ready to be written to Parquet, decoupled from the store so
/// the caller can release its lock before the (blocking) write (OBS5/M3).
pub struct PendingFlush {
    batch: RecordBatch,
    target: FlushTarget,
}

/// Where a [`PendingFlush`] is written — a local Parquet file or an
/// object-store key (H8).
enum FlushTarget {
    Local {
        data_dir: PathBuf,
        path: PathBuf,
    },
    Remote {
        store: Arc<dyn ObjectStore>,
        key: object_store::path::Path,
    },
}

/// Flush a shared store without holding its lock across the (blocking) write.
///
/// Drains the buffer under a brief write lock, releases it, then writes the
/// Parquet file on the blocking pool (OBS5/M3). Returns `true` if a file was
/// written, `false` if the buffer was empty. Extracted from the `bun`
/// collection task so the drain-then-write-off-lock sequence is unit-testable
/// instead of living only in the binary.
pub async fn flush_off_lock(
    store: &std::sync::Arc<tokio::sync::RwLock<MayoStore>>,
) -> Result<bool, MayoError> {
    let pending = {
        let mut guard = store.write().await;
        guard.take_flush_batch()?
    };
    match pending {
        Some(p) => {
            // Keep a cheap (Arc-backed) handle on the drained samples so a
            // failed write can put them back rather than lose them (M6): the
            // buffer was already cleared under the lock, so nothing else holds
            // them.
            let batch = p.batch.clone();
            match write_pending_flush(p).await {
                Ok(()) => Ok(true),
                Err(e) => {
                    store.write().await.reabsorb_batch(&batch);
                    Err(e)
                }
            }
        }
        None => Ok(false),
    }
}

/// Persist a [`PendingFlush`] to disk on the blocking pool. Runs with no lock
/// held, so concurrent queries proceed while the write is in flight.
pub async fn write_pending_flush(pending: PendingFlush) -> Result<(), MayoError> {
    let PendingFlush { batch, target } = pending;
    match target {
        FlushTarget::Local { data_dir, path } => tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&data_dir).map_err(MayoError::Io)?;
            write_batch_parquet(&path, &batch)
        })
        .await
        .map_err(|e| MayoError::Io(std::io::Error::other(e.to_string())))?,
        FlushTarget::Remote { store, key } => {
            // Encode off the runtime (Arrow's writer is blocking), then PUT the
            // bytes to the object store.
            let bytes = tokio::task::spawn_blocking(move || batch_to_parquet_bytes(&batch))
                .await
                .map_err(|e| MayoError::Io(std::io::Error::other(e.to_string())))??;
            store
                .put_opts(
                    &key,
                    object_store::PutPayload::from(bytes),
                    object_store::PutOptions {
                        mode: object_store::PutMode::Create,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| MayoError::ObjectStore(e.to_string()))?;
            Ok(())
        }
    }
}

/// Whether `data_dir` contains at least one `.parquet` file.
pub(crate) fn dir_has_parquet(data_dir: &std::path::Path) -> bool {
    std::fs::read_dir(data_dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "parquet"))
        })
        .unwrap_or(false)
}

/// A buffered sample waiting to be flushed.
struct BufferedSample {
    timestamp: u64,
    metric_name: String,
    labels_json: String,
    value: f64,
}

/// Arrow/DataFusion time-series store.
///
/// Inserts go into an in-memory buffer. On flush, the buffer is written to a
/// Parquet file and dropped from memory. Queries stream the Parquet files
/// (durable across restarts) one at a time, unioned with the unflushed
/// buffer, skipping whatever the files' statistics rule out, so a query's
/// memory is its working set regardless of how much history is on disk.
pub struct MayoStore {
    /// In-memory buffer of unflushed samples.
    buffer: Vec<BufferedSample>,
    /// Local directory for Parquet files (also the checkpoint dir under a
    /// remote backend, which otherwise ignores it).
    data_dir: PathBuf,
    /// Storage backend: a local directory or an object store (H8).
    backend: Backend,
    /// Counter for unique Parquet file names. Seeded past any existing files
    /// so a restart never clobbers a previous run's data.
    flush_counter: u64,
}

impl MayoStore {
    /// Open (or create) a store writing Parquet to a local `data_dir`.
    ///
    /// Existing `metrics_NNNNNN.parquet` files are left in place and remain
    /// queryable; the flush counter resumes past the highest one so restarts
    /// don't overwrite them.
    pub fn new(data_dir: PathBuf) -> Self {
        let flush_counter = next_flush_counter(&data_dir, "metrics");
        Self {
            buffer: Vec::new(),
            data_dir,
            backend: Backend::Local,
            flush_counter,
        }
    }

    /// Open a store, backing it with an object store when `object_store_url` is
    /// set (H8) or a local `data_dir` otherwise. Generic opens read the whole
    /// configured archive. Remote chunks use fresh random 128-bit names and
    /// create-only PUTs, so simultaneous writers never replace existing data.
    pub async fn open(
        data_dir: PathBuf,
        object_store_url: Option<&str>,
    ) -> Result<Self, MayoError> {
        let Some(url) = object_store_url.filter(|u| !u.is_empty()) else {
            return Ok(Self::new(data_dir));
        };
        let (store, prefix) = parse_object_store(url)?;
        let flush_counter = 0;
        Ok(Self {
            buffer: Vec::new(),
            data_dir,
            backend: Backend::Remote { store, prefix },
            flush_counter,
        })
    }

    /// Open a production node's archive within the configured bucket. The
    /// stable opaque owner is hashed, so labels and path characters cannot
    /// accidentally merge writers. Generic `open` still reads the full archive.
    pub async fn open_for_node(
        data_dir: PathBuf,
        object_store_url: Option<&str>,
        owner: &str,
    ) -> Result<Self, MayoError> {
        let mut store = Self::open(data_dir, object_store_url).await?;
        if let Backend::Remote { prefix, .. } = &mut store.backend {
            if owner.is_empty() {
                return Err(MayoError::ObjectStore(
                    "node archive owner is required".into(),
                ));
            }
            *prefix = prefix
                .clone()
                .join(format!("nodes/{:x}", Sha256::digest(owner.as_bytes())).as_str());
        }
        Ok(store)
    }

    /// The local directory where Parquet files are stored (the configured
    /// metrics dir; unused for storage under a remote backend).
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Insert a metric sample into the buffer.
    pub fn insert(&mut self, key: &MetricKey, sample: Sample) {
        self.buffer.push(BufferedSample {
            timestamp: sample.timestamp,
            metric_name: key.name.0.clone(),
            labels_json: key.labels_json(),
            value: sample.value,
        });
    }

    /// Insert with the current timestamp (convenience).
    pub fn insert_now(&mut self, key: &MetricKey, value: f64) {
        self.insert(key, Sample::now(value));
    }

    /// Number of unflushed samples in the buffer.
    pub fn buffer_len(&self) -> usize {
        self.buffer.len()
    }

    /// Convert the buffer to an Arrow RecordBatch.
    fn buffer_to_batch(&self) -> Result<Option<RecordBatch>, MayoError> {
        if self.buffer.is_empty() {
            return Ok(None);
        }

        let timestamps: Vec<u64> = self.buffer.iter().map(|s| s.timestamp).collect();
        let names: Vec<&str> = self.buffer.iter().map(|s| s.metric_name.as_str()).collect();
        let labels: Vec<&str> = self.buffer.iter().map(|s| s.labels_json.as_str()).collect();
        let values: Vec<f64> = self.buffer.iter().map(|s| s.value).collect();

        let batch = RecordBatch::try_new(
            Arc::new(metrics_schema()),
            vec![
                Arc::new(UInt64Array::from(timestamps)),
                Arc::new(StringArray::from(names)),
                Arc::new(StringArray::from(labels)),
                Arc::new(Float64Array::from(values)),
            ],
        )
        .map_err(|e| MayoError::Arrow(e.to_string()))?;

        Ok(Some(batch))
    }

    /// Flush the buffer: convert to RecordBatch, write Parquet, drop from
    /// memory. The write runs on the blocking pool (OBS5/M3).
    ///
    /// This convenience keeps the whole operation under `&mut self`. Callers
    /// holding a shared lock across many concurrent readers should instead use
    /// [`take_flush_batch`](Self::take_flush_batch) + [`write_pending_flush`]
    /// so the lock is released during the I/O and queries never starve.
    pub async fn flush(&mut self) -> Result<(), MayoError> {
        let Some(pending) = self.take_flush_batch()? else {
            return Ok(());
        };
        let batch = pending.batch.clone();
        if let Err(e) = write_pending_flush(pending).await {
            // A failed write must not discard the drained samples (M6).
            self.reabsorb_batch(&batch);
            return Err(e);
        }
        Ok(())
    }

    /// Put a drained flush batch back into the buffer after a failed write, so
    /// the samples are retried on the next flush instead of lost (M6). Rows are
    /// appended; ordering doesn't matter (queries sort by timestamp).
    pub(crate) fn reabsorb_batch(&mut self, batch: &RecordBatch) {
        let (Some(timestamps), Some(names), Some(labels), Some(values)) = (
            batch.column(0).as_any().downcast_ref::<UInt64Array>(),
            batch.column(1).as_any().downcast_ref::<StringArray>(),
            batch.column(2).as_any().downcast_ref::<StringArray>(),
            batch.column(3).as_any().downcast_ref::<Float64Array>(),
        ) else {
            eprintln!("mayo: could not re-buffer a failed flush batch (schema mismatch)");
            return;
        };
        for i in 0..batch.num_rows() {
            self.buffer.push(BufferedSample {
                timestamp: timestamps.value(i),
                metric_name: names.value(i).to_string(),
                labels_json: labels.value(i).to_string(),
                value: values.value(i),
            });
        }
    }

    /// Drain the buffer into a self-contained [`PendingFlush`] the caller writes
    /// later, outside any lock. Bumps the flush counter and clears the buffer
    /// immediately, so the on-disk file name is reserved before the (slow)
    /// write. Returns `None` when there's nothing to flush.
    pub fn take_flush_batch(&mut self) -> Result<Option<PendingFlush>, MayoError> {
        let Some(batch) = self.buffer_to_batch()? else {
            return Ok(None);
        };
        let filename = format!("metrics_{:06}.parquet", self.flush_counter);
        self.buffer.clear();
        self.flush_counter += 1;
        let target = match &self.backend {
            Backend::Local => FlushTarget::Local {
                data_dir: self.data_dir.clone(),
                path: self.data_dir.join(&filename),
            },
            Backend::Remote { store, prefix, .. } => FlushTarget::Remote {
                store: Arc::clone(store),
                key: prefix
                    .clone()
                    .join(format!("metrics_{:032x}.parquet", rand::random::<u128>()).as_str()),
            },
        };
        Ok(Some(PendingFlush { batch, target }))
    }

    /// Build a DataFusion session exposing a `metrics` table over all data:
    /// the Parquet files unioned with the unflushed buffer.
    async fn session(&self) -> Result<SessionContext, MayoError> {
        self.session_since(None).await
    }

    /// [`session`](Self::session) for a query whose SQL only wants samples at
    /// or after `since`.
    ///
    /// The `metrics` table is streamed (#377): nothing is read here beyond a
    /// listing of file names. Each query reads its files one at a time, skips
    /// files and row groups whose footer statistics rule out its time range
    /// or metric names, and decodes only the columns it uses. `since` is one
    /// more lower bound for that pruning; the SQL's own `timestamp`
    /// predicates already give the same, so it's a promise, not a filter.
    async fn session_since(&self, since: Option<u64>) -> Result<SessionContext, MayoError> {
        let ctx = streaming_session();
        let sources = match &self.backend {
            Backend::Local => list_local(&self.data_dir)?,
            Backend::Remote { store, prefix } => list_remote(store, prefix, "metrics").await?,
        };
        let table = ParquetTable::new(
            Arc::new(metrics_schema()),
            sources,
            self.buffer_to_batch()?,
            since,
            "metrics",
        );
        ctx.register_table("metrics", Arc::new(table))
            .map_err(|e| MayoError::DataFusion(e.to_string()))?;
        Ok(ctx)
    }

    /// Query metrics using SQL. Returns (timestamp, name, labels, value) tuples.
    pub async fn query_sql(&self, sql: &str) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        self.query_rows(sql, None).await
    }

    /// [`query_sql`](Self::query_sql) for SQL that only selects samples with
    /// `timestamp >= since`. Files entirely older than `since` aren't read,
    /// so the SQL must filter on that bound itself or it would see a subset.
    pub async fn query_sql_since(
        &self,
        sql: &str,
        since: u64,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        self.query_rows(sql, Some(since)).await
    }

    async fn query_rows(
        &self,
        sql: &str,
        since: Option<u64>,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        let ctx = self.session_since(since).await?;
        let df = ctx
            .sql(sql)
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        let batches = df
            .collect()
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        let mut results = Vec::new();
        for batch in &batches {
            if batch.num_columns() < 4 {
                continue;
            }
            let timestamps = batch
                .column(0)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .ok_or_else(|| MayoError::Arrow("timestamp column type mismatch".into()))?;
            let names = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| MayoError::Arrow("metric_name column type mismatch".into()))?;
            let labels = batch
                .column(2)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| MayoError::Arrow("labels column type mismatch".into()))?;
            let values = batch
                .column(3)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| MayoError::Arrow("value column type mismatch".into()))?;

            for i in 0..batch.num_rows() {
                results.push((
                    timestamps.value(i),
                    names.value(i).to_string(),
                    labels.value(i).to_string(),
                    values.value(i),
                ));
            }
        }

        Ok(results)
    }

    /// One app's samples in `[start, end]`, oldest first, newest kept.
    ///
    /// `app_label` is matched against label values (the `namespace/app`
    /// every per-app sample carries) and `name`, when given, against the
    /// metric name. At most [`APP_QUERY_ROW_LIMIT`] rows come back, and they
    /// are the newest ones: a long window loses its oldest samples, never the
    /// latest. `per_series` keeps only the newest N samples of each series
    /// (metric name plus labels), which is how a caller asks for "the latest
    /// value of everything" without paying for the whole window.
    pub async fn query_app(
        &self,
        app_label: &str,
        name: Option<&str>,
        start: u64,
        end: u64,
        per_series: Option<u32>,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        let app_filter = escape_sql_literal(app_label);
        let name_filter = name
            .map(|name| format!("metric_name = '{}' AND ", escape_sql_literal(name)))
            .unwrap_or_default();
        let filter = format!(
            "{name_filter}labels LIKE '%\"{app_filter}\"%' \
             AND timestamp >= {start} AND timestamp <= {end}"
        );
        let sql = match per_series {
            None => format!(
                "SELECT timestamp, metric_name, labels, value FROM metrics \
                 WHERE {filter} ORDER BY timestamp DESC LIMIT {APP_QUERY_ROW_LIMIT}"
            ),
            Some(keep) => format!(
                "SELECT timestamp, metric_name, labels, value FROM ( \
                   SELECT timestamp, metric_name, labels, value, \
                     ROW_NUMBER() OVER ( \
                       PARTITION BY metric_name, labels ORDER BY timestamp DESC \
                     ) AS series_rank \
                   FROM metrics WHERE {filter} \
                 ) WHERE series_rank <= {keep} \
                 ORDER BY timestamp DESC LIMIT {APP_QUERY_ROW_LIMIT}"
            ),
        };
        let mut rows = self.query_sql_since(&sql, start).await?;
        rows.reverse();
        Ok(rows)
    }

    /// Query by metric name and time range (convenience).
    pub async fn query(
        &self,
        metric_name: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        let metric_name = escape_sql_literal(metric_name);
        let sql = format!(
            "SELECT timestamp, metric_name, labels, value FROM metrics \
             WHERE metric_name = '{metric_name}' \
             AND timestamp >= {start} AND timestamp <= {end} \
             ORDER BY timestamp"
        );
        self.query_sql_since(&sql, start).await
    }

    /// Every series' samples in `[start, end]`, oldest first, at most
    /// [`ALL_QUERY_ROW_LIMIT`] of them: what `/v1/metrics?name=*` returns.
    pub async fn query_all(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        let sql = format!(
            "SELECT timestamp, metric_name, labels, value FROM metrics \
             WHERE timestamp >= {start} AND timestamp <= {end} \
             ORDER BY timestamp LIMIT {ALL_QUERY_ROW_LIMIT}"
        );
        self.query_sql_since(&sql, start).await
    }

    /// Samples of the named metrics at or after `since`, oldest first. `relish
    /// top` reads its CPU and memory columns this way.
    pub async fn query_names_since(
        &self,
        names: &[&str],
        since: u64,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        let names = names
            .iter()
            .map(|name| format!("'{}'", escape_sql_literal(name)))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT timestamp, metric_name, labels, value FROM metrics \
             WHERE metric_name IN ({names}) \
             AND timestamp >= {since} ORDER BY timestamp"
        );
        self.query_sql_since(&sql, since).await
    }

    /// Query the average value of a metric over a time window.
    ///
    /// Used by the autoscaler to compute average CPU/memory utilisation.
    /// The `app_label` filters by the `app` label in the metrics labels JSON.
    /// Returns `None` if no data points exist in the window.
    pub async fn query_avg(
        &self,
        metric_name: &str,
        app_label: &str,
        window_secs: u64,
    ) -> Result<Option<f64>, MayoError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let start = now.saturating_sub(window_secs);

        let metric_name = escape_sql_literal(metric_name);
        let app_label = escape_sql_literal(app_label);
        let sql = format!(
            "SELECT AVG(value) as avg_val FROM metrics \
             WHERE metric_name = '{metric_name}' \
             AND labels LIKE '%\"{app_label}\"%' \
             AND timestamp >= {start} AND timestamp <= {now}"
        );

        let ctx = self.session_since(Some(start)).await?;
        let df = ctx
            .sql(&sql)
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        let batches = df
            .collect()
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        for batch in &batches {
            if batch.num_rows() == 0 || batch.num_columns() == 0 {
                continue;
            }
            if let Some(col) = batch.column(0).as_any().downcast_ref::<Float64Array>()
                && !col.is_null(0)
            {
                return Ok(Some(col.value(0)));
            }
        }

        Ok(None)
    }

    /// List all distinct metric names.
    pub async fn metric_names(&self) -> Result<Vec<String>, MayoError> {
        let ctx = self.session().await?;
        let df = ctx
            .sql("SELECT DISTINCT metric_name FROM metrics ORDER BY metric_name")
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        let batches = df
            .collect()
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        let mut names = Vec::new();
        for batch in &batches {
            let col = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| MayoError::Arrow("column type mismatch".into()))?;
            for i in 0..batch.num_rows() {
                names.push(col.value(i).to_string());
            }
        }

        Ok(names)
    }

    /// Query aggregated statistics for all metrics in a time window.
    ///
    /// Returns (metric_name, labels_json, min, max, sum, count) tuples,
    /// one per distinct (metric_name, labels) combination. Used by the
    /// rollup generator to build `NodeRollup` entries.
    pub async fn query_window_aggregates(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Vec<(String, String, f64, f64, f64, u32)>, MayoError> {
        let sql = format!(
            "SELECT metric_name, labels, \
             MIN(value) as min_val, MAX(value) as max_val, \
             SUM(value) as sum_val, COUNT(*) as count_val \
             FROM metrics \
             WHERE timestamp >= {start} AND timestamp < {end} \
             GROUP BY metric_name, labels \
             ORDER BY metric_name, labels"
        );

        let ctx = self.session_since(Some(start)).await?;
        let df = ctx
            .sql(&sql)
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        let batches = df
            .collect()
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        let mut results = Vec::new();
        for batch in &batches {
            let names = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| MayoError::Arrow("metric_name column type mismatch".into()))?;
            let labels = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| MayoError::Arrow("labels column type mismatch".into()))?;
            let mins = batch
                .column(2)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| MayoError::Arrow("min column type mismatch".into()))?;
            let maxs = batch
                .column(3)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| MayoError::Arrow("max column type mismatch".into()))?;
            let sums = batch
                .column(4)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| MayoError::Arrow("sum column type mismatch".into()))?;
            // COUNT(*) returns i64 in DataFusion
            let counts = batch
                .column(5)
                .as_any()
                .downcast_ref::<datafusion::arrow::array::Int64Array>()
                .ok_or_else(|| MayoError::Arrow("count column type mismatch".into()))?;

            for i in 0..batch.num_rows() {
                results.push((
                    names.value(i).to_string(),
                    labels.value(i).to_string(),
                    mins.value(i),
                    maxs.value(i),
                    sums.value(i),
                    counts.value(i) as u32,
                ));
            }
        }

        Ok(results)
    }

    /// Prune Parquet files whose newest datapoint is older than `before`.
    ///
    /// Retention is keyed on the data's own newest timestamp (O12), read from
    /// the file's row-group statistics — not the file's mtime, which a
    /// touch/copy or clock skew can push forward and so drop in-range data. A
    /// file whose max timestamp can't be read falls back to mtime so a
    /// malformed file is still eligible for pruning.
    pub fn prune(&self, before: u64) -> Result<usize, MayoError> {
        // Remote backends leave retention to the bucket's own lifecycle policy
        // (the idiomatic way to expire object-store data), so node-side pruning
        // is a no-op there (H8).
        if let Backend::Remote { .. } = &self.backend {
            return Ok(0);
        }
        let mut deleted = 0;
        let entries = match std::fs::read_dir(&self.data_dir) {
            Ok(entries) => entries,
            // A new store has no directory until its first flush.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(MayoError::Io(std::io::Error::new(
                    error.kind(),
                    format!("list metrics {}: {error}", self.data_dir.display()),
                )));
            }
        };
        for entry in entries {
            let path = entry
                .map_err(|error| {
                    MayoError::Io(std::io::Error::new(
                        error.kind(),
                        format!(
                            "read metrics directory {}: {error}",
                            self.data_dir.display()
                        ),
                    ))
                })?
                .path();
            if !path.extension().is_some_and(|e| e == "parquet") {
                continue;
            }
            let newest = file_max_timestamp(&path).unwrap_or_else(|| {
                std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|t| {
                        t.duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                    })
                    .unwrap_or(u64::MAX)
            });
            if newest < before {
                match std::fs::remove_file(&path) {
                    Ok(()) => deleted += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(MayoError::Io(std::io::Error::new(
                            error.kind(),
                            format!("prune {}: {error}", path.display()),
                        )));
                    }
                }
            }
        }
        Ok(deleted)
    }
}

/// The maximum `timestamp` (column 0) across a Parquet file's row groups, read
/// from statistics without scanning the data. `None` if the file is unreadable
/// or carries no usable stats.
pub(crate) fn file_max_timestamp(path: &Path) -> Option<u64> {
    use datafusion::parquet::file::reader::{FileReader, SerializedFileReader};
    use datafusion::parquet::file::statistics::Statistics;

    let reader = SerializedFileReader::new(std::fs::File::open(path).ok()?).ok()?;
    let meta = reader.metadata();
    let mut max: Option<u64> = None;
    for i in 0..meta.num_row_groups() {
        let rg = meta.row_group(i);
        if rg.num_columns() == 0 {
            continue;
        }
        let candidate = match rg.column(0).statistics() {
            Some(Statistics::Int64(s)) => s.max_opt().map(|v| *v as u64),
            Some(Statistics::Int32(s)) => s.max_opt().map(|v| *v as u64),
            _ => None,
        };
        if let Some(c) = candidate {
            max = Some(max.map_or(c, |cur| cur.max(c)));
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mayo::types::MetricKey;

    #[test]
    fn prune_reports_failed_deletions_and_preserves_survivors() {
        let (store, dir) = test_store();
        let blocked = dir.path().join("blocked.parquet");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("survivor"), b"preserve").unwrap();
        let error = store
            .prune(u64::MAX)
            .expect_err("failed removal must not count as deletion");
        assert!(error.to_string().contains("blocked.parquet"), "{error}");
        assert_eq!(
            std::fs::read(blocked.join("survivor")).unwrap(),
            b"preserve"
        );
    }

    fn app_key(name: &str, app: &str, instance: &str) -> MetricKey {
        MetricKey::with_labels(
            name,
            std::collections::BTreeMap::from([
                ("app".to_string(), app.to_string()),
                ("instance".to_string(), instance.to_string()),
            ]),
        )
    }

    /// The old per-app query ordered ascending from `start` and cut at the
    /// row limit, so a busy app's newest samples, the ones every "latest"
    /// view wants, were the first thing dropped.
    #[tokio::test]
    async fn app_query_keeps_the_newest_rows_when_over_the_limit() {
        let (mut store, _dir) = test_store();
        let key = app_key("requests_total", "default/web", "web-0");
        let total = APP_QUERY_ROW_LIMIT as u64 + 500;
        for timestamp in 1..=total {
            store.insert(&key, Sample::at(timestamp, timestamp as f64));
        }
        let rows = store
            .query_app("default/web", Some("requests_total"), 0, total, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), APP_QUERY_ROW_LIMIT);
        assert_eq!(rows.last().unwrap().0, total, "the newest sample was lost");
        assert!(
            rows.windows(2).all(|pair| pair[0].0 <= pair[1].0),
            "rows must come back oldest first"
        );
    }

    #[tokio::test]
    async fn app_query_per_series_keeps_the_newest_n_of_each_series() {
        let (mut store, _dir) = test_store();
        for instance in ["web-0", "web-1"] {
            let key = app_key("requests_total", "default/web", instance);
            for timestamp in 1..=5 {
                store.insert(&key, Sample::at(timestamp, timestamp as f64));
            }
        }
        store.insert(
            &app_key("requests_total", "default/other", "other-0"),
            Sample::at(5, 99.0),
        );
        let rows = store
            .query_app("default/web", None, 0, 10, Some(2))
            .await
            .unwrap();
        let mut seen: Vec<(u64, String)> = rows
            .iter()
            .map(|(timestamp, _, labels, _)| (*timestamp, labels.clone()))
            .collect();
        seen.sort();
        assert_eq!(rows.len(), 4, "{seen:?}");
        assert!(rows.iter().all(|(timestamp, ..)| *timestamp >= 4));
        assert!(
            rows.iter()
                .all(|(_, _, labels, _)| !labels.contains("other"))
        );
    }

    #[tokio::test]
    async fn app_query_escapes_the_app_and_name() {
        let (mut store, _dir) = test_store();
        store.insert(&app_key("m", "default/web", "web-0"), Sample::at(1, 1.0));
        let rows = store
            .query_app("x' OR '1'='1", Some("m' OR '1'='1"), 0, 10, Some(1))
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    fn test_store() -> (MayoStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = MayoStore::new(dir.path().to_path_buf());
        (store, dir)
    }

    /// H8: with an object-store backend (exercised here via a `file://` temp
    /// dir — object_store's LocalFileSystem — so no real cloud is needed),
    /// flushed metrics land in the store and the same queries read them back,
    /// including across a "restart" that re-opens the store and resumes the
    /// flush counter from what's already there.
    #[tokio::test]
    async fn object_store_backend_round_trips_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let url = url::Url::from_file_path(dir.path()).unwrap().to_string();

        let mut store = MayoStore::open(dir.path().to_path_buf(), Some(&url))
            .await
            .unwrap();
        let key = MetricKey::simple("node_cpu");
        store.insert(&key, Sample::at(1000, 10.0));
        store.insert(&key, Sample::at(1001, 20.0));
        store.flush().await.unwrap();

        let rows = store.query("node_cpu", 0, 2000).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].3, 10.0);

        // Re-open (restart): the store lists existing objects, so a fresh flush
        // doesn't clobber the first file, and both are queryable.
        let mut restarted = MayoStore::open(dir.path().to_path_buf(), Some(&url))
            .await
            .unwrap();
        restarted.insert(&key, Sample::at(1002, 30.0));
        restarted.flush().await.unwrap();
        let rows = restarted.query("node_cpu", 0, 2000).await.unwrap();
        assert_eq!(rows.len(), 3, "restart must not clobber earlier objects");

        // Prune is a no-op for a remote backend (bucket lifecycle owns retention).
        assert_eq!(restarted.prune(u64::MAX).unwrap(), 0);
    }

    #[tokio::test]
    async fn reabsorb_batch_restores_drained_samples() {
        // M6: a failed write must not lose the drained samples. take_flush_batch
        // clears the buffer; reabsorb_batch puts the rows back so the next flush
        // retries them.
        let (mut store, _dir) = test_store();
        let key = MetricKey::simple("cpu");
        store.insert(&key, Sample::at(1, 1.0));
        store.insert(&key, Sample::at(2, 2.0));
        let pending = store.take_flush_batch().unwrap().unwrap();
        assert_eq!(store.buffer_len(), 0, "take_flush_batch drains the buffer");
        store.reabsorb_batch(&pending.batch);
        assert_eq!(store.buffer_len(), 2, "the samples are back for a retry");
        // And they still flush and query correctly afterwards.
        store.flush().await.unwrap();
        let rows = store.query("cpu", 0, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn insert_and_flush_creates_parquet() {
        let (mut store, dir) = test_store();
        let key = MetricKey::simple("cpu_usage");
        store.insert(&key, Sample::at(1000, 42.5));
        store.insert(&key, Sample::at(1001, 43.0));

        store.flush().await.unwrap();

        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "parquet"))
            .collect();
        assert_eq!(files.len(), 1);
    }

    /// O12: retention keys on the data's own newest timestamp, not the file's
    /// mtime. A file just written (recent mtime) but holding only old data is
    /// pruned; a file holding recent data is kept regardless of mtime.
    #[tokio::test]
    async fn prune_uses_data_timestamp_not_file_mtime() {
        let (mut store, dir) = test_store();
        // Freshly written file (mtime ~now), but its newest datapoint is ts=5.
        store.insert(&MetricKey::simple("cpu"), Sample::at(5, 1.0));
        store.flush().await.unwrap();

        // mtime-based pruning would keep this (mtime is now); timestamp-based
        // prunes it (data max 5 < 100).
        assert_eq!(store.prune(100).unwrap(), 1);
        assert!(!dir_has_parquet(dir.path()));

        // A file with recent data survives the same cutoff.
        store.insert(&MetricKey::simple("cpu"), Sample::at(1_000, 2.0));
        store.flush().await.unwrap();
        assert_eq!(store.prune(100).unwrap(), 0);
        assert!(dir_has_parquet(dir.path()));
    }

    #[tokio::test]
    async fn query_after_flush() {
        let (mut store, _dir) = test_store();
        let key = MetricKey::simple("cpu_usage");
        store.insert(&key, Sample::at(1000, 42.5));
        store.insert(&key, Sample::at(1001, 43.0));
        store.insert(&key, Sample::at(1002, 44.0));
        store.flush().await.unwrap();

        let results = store.query("cpu_usage", 1000, 1002).await.unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, 1000);
        assert_eq!(results[0].3, 42.5);
    }

    #[tokio::test]
    async fn query_time_range_filters() {
        let (mut store, _dir) = test_store();
        let key = MetricKey::simple("mem");
        store.insert(&key, Sample::at(100, 1.0));
        store.insert(&key, Sample::at(200, 2.0));
        store.insert(&key, Sample::at(300, 3.0));
        store.flush().await.unwrap();

        let results = store.query("mem", 150, 250).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].3, 2.0);
    }

    /// #310: a windowed read skips files wholly before its window, but a file
    /// whose newest sample sits exactly on the window's start still counts,
    /// and so does one that only straddles it.
    #[tokio::test]
    async fn windowed_reads_keep_every_file_that_reaches_the_window() {
        let (mut store, _dir) = test_store();
        let key = MetricKey::simple("mem");
        for file in [[100, 150], [180, 200], [190, 260]] {
            for timestamp in file {
                store.insert(&key, Sample::at(timestamp, timestamp as f64));
            }
            store.flush().await.unwrap();
        }
        store.insert(&key, Sample::at(300, 300.0));

        let rows = store.query("mem", 200, 400).await.unwrap();
        let seen: Vec<u64> = rows.iter().map(|row| row.0).collect();
        assert_eq!(seen, vec![200, 260, 300]);
        let since = store
            .query_sql_since(
                "SELECT timestamp, metric_name, labels, value FROM metrics \
                 WHERE timestamp >= 150 ORDER BY timestamp",
                150,
            )
            .await
            .unwrap();
        let seen: Vec<u64> = since.iter().map(|row| row.0).collect();
        assert_eq!(seen, vec![150, 180, 190, 200, 260, 300]);
    }

    #[tokio::test]
    async fn query_nonexistent_metric_returns_empty() {
        let (mut store, _dir) = test_store();
        let key = MetricKey::simple("cpu");
        store.insert(&key, Sample::at(1000, 1.0));
        store.flush().await.unwrap();

        let results = store.query("nonexistent", 0, 9999).await.unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn escape_sql_literal_doubles_quotes() {
        assert_eq!(escape_sql_literal("cpu_usage"), "cpu_usage");
        assert_eq!(escape_sql_literal("a'b"), "a''b");
        assert_eq!(escape_sql_literal("x' OR '1'='1"), "x'' OR ''1''=''1");
    }

    /// M1: an injection payload in the metric name must not break out of
    /// the SQL literal and leak another metric's rows.
    #[tokio::test]
    async fn query_metric_name_injection_is_neutralised() {
        let (mut store, _dir) = test_store();
        store.insert(&MetricKey::simple("secret"), Sample::at(1000, 9.9));
        store.flush().await.unwrap();

        // Classic `' OR '1'='1` — if unescaped it would return every row.
        let results = store.query("x' OR '1'='1", 0, 9999).await.unwrap();
        assert!(results.is_empty(), "SQL injection leaked rows: {results:?}");
    }

    #[tokio::test]
    async fn query_with_labels() {
        let (mut store, _dir) = test_store();
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("app".to_string(), "web".to_string());
        let key = MetricKey::with_labels("requests", labels);
        store.insert(&key, Sample::at(1000, 100.0));
        store.flush().await.unwrap();

        let results = store
            .query_sql(
                "SELECT timestamp, metric_name, labels, value FROM metrics \
                 WHERE metric_name = 'requests' AND labels LIKE '%web%'",
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].3, 100.0);
    }

    #[tokio::test]
    async fn multiple_metrics_in_same_store() {
        let (mut store, _dir) = test_store();
        store.insert(&MetricKey::simple("cpu"), Sample::at(1000, 50.0));
        store.insert(&MetricKey::simple("mem"), Sample::at(1000, 1024.0));
        store.flush().await.unwrap();

        let cpu = store.query("cpu", 0, 9999).await.unwrap();
        let mem = store.query("mem", 0, 9999).await.unwrap();
        assert_eq!(cpu.len(), 1);
        assert_eq!(mem.len(), 1);
        assert_eq!(cpu[0].3, 50.0);
        assert_eq!(mem[0].3, 1024.0);
    }

    #[tokio::test]
    async fn metric_names_lists_distinct() {
        let (mut store, _dir) = test_store();
        store.insert(&MetricKey::simple("beta"), Sample::at(1, 1.0));
        store.insert(&MetricKey::simple("alpha"), Sample::at(1, 2.0));
        store.insert(&MetricKey::simple("beta"), Sample::at(2, 3.0));
        store.flush().await.unwrap();

        let names = store.metric_names().await.unwrap();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[tokio::test]
    async fn flush_empty_buffer_is_noop() {
        let (mut store, _dir) = test_store();
        store.flush().await.unwrap();
    }

    #[tokio::test]
    async fn buffer_len_tracks_inserts() {
        let (mut store, _dir) = test_store();
        assert_eq!(store.buffer_len(), 0);
        store.insert(&MetricKey::simple("x"), Sample::at(1, 1.0));
        assert_eq!(store.buffer_len(), 1);
    }

    #[tokio::test]
    async fn flush_clears_buffer() {
        let (mut store, _dir) = test_store();
        store.insert(&MetricKey::simple("x"), Sample::at(1, 1.0));
        store.flush().await.unwrap();
        assert_eq!(store.buffer_len(), 0);
    }

    #[tokio::test]
    async fn multiple_flushes_queryable() {
        let (mut store, _dir) = test_store();
        store.insert(&MetricKey::simple("a"), Sample::at(1, 1.0));
        store.flush().await.unwrap();

        store.insert(&MetricKey::simple("b"), Sample::at(2, 2.0));
        store.flush().await.unwrap();

        let names = store.metric_names().await.unwrap();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn arrow_schema_has_expected_columns() {
        let schema = metrics_schema();
        assert_eq!(schema.fields().len(), 4);
        assert_eq!(schema.field(0).name(), "timestamp");
        assert_eq!(schema.field(1).name(), "metric_name");
        assert_eq!(schema.field(2).name(), "labels");
        assert_eq!(schema.field(3).name(), "value");
    }

    #[tokio::test]
    async fn query_empty_store_returns_empty() {
        let (store, _dir) = test_store();
        let results = store.query("anything", 0, 9999).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn query_unflushed_buffer_visible() {
        let (mut store, _dir) = test_store();
        let key = MetricKey::simple("live_metric");
        store.insert(&key, Sample::at(1000, 42.0));
        // Don't flush — query should still see buffer data
        let results = store.query("live_metric", 0, 9999).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].3, 42.0);
    }

    #[tokio::test]
    async fn reopen_reads_persisted_parquet_without_clobbering() {
        let dir = tempfile::tempdir().unwrap();

        // First run: flush two separate files.
        {
            let mut store = MayoStore::new(dir.path().to_path_buf());
            store.insert(&MetricKey::simple("cpu"), Sample::at(1, 10.0));
            store.flush().await.unwrap();
            store.insert(&MetricKey::simple("cpu"), Sample::at(2, 20.0));
            store.flush().await.unwrap();
        }
        let files_after_first = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "parquet")
            })
            .count();
        assert_eq!(files_after_first, 2);

        // Second run over the same dir: prior data is queryable...
        let mut store = MayoStore::new(dir.path().to_path_buf());
        let results = store.query("cpu", 0, 9999).await.unwrap();
        assert_eq!(
            results.len(),
            2,
            "persisted data not reloaded after restart"
        );

        // ...and a new flush appends a third file, not overwriting file 000000.
        store.insert(&MetricKey::simple("cpu"), Sample::at(3, 30.0));
        store.flush().await.unwrap();
        let files_after_second = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "parquet")
            })
            .count();
        assert_eq!(files_after_second, 3, "restart clobbered an existing file");

        let all = store.query("cpu", 0, 9999).await.unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn query_sees_both_flushed_and_unflushed() {
        let (mut store, _dir) = test_store();
        store.insert(&MetricKey::simple("m"), Sample::at(1, 10.0));
        store.flush().await.unwrap();

        store.insert(&MetricKey::simple("m"), Sample::at(2, 20.0));
        // Second sample not flushed

        let results = store.query("m", 0, 9999).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].3, 10.0);
        assert_eq!(results[1].3, 20.0);
    }

    #[tokio::test]
    async fn query_proceeds_during_flush() {
        // OBS5: the flush I/O must not hold the store lock. We drain the buffer
        // under a brief write lock, release it, then run the (blocking) write
        // and a concurrent read at the same time. If the write held the lock,
        // the read would block until it finished; because it doesn't, both
        // complete together. No sleep — `join!` drives both to completion and
        // the read asserting the flushed row proves it observed a consistent
        // store while the write was in flight.
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(RwLock::new(MayoStore::new(dir.path().to_path_buf())));

        // Seed and flush one row so a query has something to read.
        {
            let mut s = store.write().await;
            s.insert(&MetricKey::simple("m"), Sample::at(1, 10.0));
            let pending = s.take_flush_batch().unwrap().unwrap();
            drop(s); // lock released before the write
            write_pending_flush(pending).await.unwrap();
        }

        // Now stage a second flush and run its write concurrently with a query.
        let pending = {
            let mut s = store.write().await;
            s.insert(&MetricKey::simple("m"), Sample::at(2, 20.0));
            s.take_flush_batch().unwrap().unwrap()
        }; // write lock dropped here — the write below holds no store lock

        let read_store = Arc::clone(&store);
        let (write_res, read_res) = tokio::join!(write_pending_flush(pending), async move {
            let s = read_store.read().await;
            s.query("m", 0, 9999).await
        });
        write_res.unwrap();
        // The read ran against the store while the flush write was in flight and
        // returned the already-persisted first row without blocking.
        let rows = read_res.unwrap();
        assert!(
            rows.iter().any(|r| r.3 == 10.0),
            "concurrent query did not see persisted data: {rows:?}"
        );
    }

    #[tokio::test]
    async fn corrupt_parquet_file_does_not_fail_query() {
        // OBS5: a truncated/garbage Parquet file must be skipped on read, not
        // fail an unrelated query.
        let (mut store, dir) = test_store();
        store.insert(&MetricKey::simple("cpu"), Sample::at(1, 10.0));
        store.flush().await.unwrap();

        std::fs::write(dir.path().join("metrics_999999.parquet"), b"garbage").unwrap();

        let results = store.query("cpu", 0, 9999).await.unwrap();
        assert_eq!(results.len(), 1, "corrupt file broke an unrelated query");
        assert_eq!(results[0].3, 10.0);
    }

    #[tokio::test]
    async fn flush_off_lock_writes_and_reports_emptiness() {
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(RwLock::new(MayoStore::new(dir.path().to_path_buf())));

        // Empty buffer: nothing written, reports false.
        assert!(!flush_off_lock(&store).await.unwrap());

        // With data: writes one Parquet file, reports true, clears the buffer.
        store
            .write()
            .await
            .insert(&MetricKey::simple("cpu"), Sample::at(1, 7.0));
        assert!(flush_off_lock(&store).await.unwrap());
        assert_eq!(store.read().await.buffer_len(), 0);

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
        assert_eq!(files, 1);

        // The data is queryable afterwards.
        let rows = store.read().await.query("cpu", 0, 9999).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].3, 7.0);
    }

    #[tokio::test]
    async fn prune_removes_old_parquet_files() {
        let (mut store, dir) = test_store();
        store.insert(&MetricKey::simple("cpu"), Sample::at(1, 10.0));
        store.flush().await.unwrap();
        assert!(dir_has_parquet(dir.path()));

        // A `before` far in the future prunes every file (they're older).
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 10_000;
        let deleted = store.prune(future).unwrap();
        assert_eq!(deleted, 1);
        assert!(!dir_has_parquet(dir.path()));
    }

    #[tokio::test]
    async fn prune_keeps_recent_parquet_files() {
        let (mut store, dir) = test_store();
        store.insert(&MetricKey::simple("cpu"), Sample::at(1, 10.0));
        store.flush().await.unwrap();

        // A `before` of 0 keeps everything (nothing is older than the epoch).
        let deleted = store.prune(0).unwrap();
        assert_eq!(deleted, 0);
        assert!(dir_has_parquet(dir.path()));
    }

    #[tokio::test]
    async fn query_avg_filters_by_app_label_and_window() {
        let (mut store, _dir) = test_store();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut web = std::collections::BTreeMap::new();
        web.insert("app".to_string(), "web".to_string());
        let web_key = MetricKey::with_labels("cpu", web);
        store.insert(&web_key, Sample::at(now - 5, 10.0));
        store.insert(&web_key, Sample::at(now - 4, 30.0));

        let mut other = std::collections::BTreeMap::new();
        other.insert("app".to_string(), "other".to_string());
        let other_key = MetricKey::with_labels("cpu", other);
        store.insert(&other_key, Sample::at(now - 5, 100.0));
        store.flush().await.unwrap();

        // Average across web's two samples only: (10 + 30) / 2 = 20.
        let avg = store.query_avg("cpu", "web", 60).await.unwrap();
        assert_eq!(avg, Some(20.0));

        // No data for an unknown app in the window → None.
        let none = store.query_avg("cpu", "ghost", 60).await.unwrap();
        assert_eq!(none, None);
    }

    /// Rows in a canonical order, one per line, so a snapshot compares sets
    /// whatever order files were listed in. The caller checks the ordering
    /// it was promised separately.
    fn canonical(rows: &[(u64, String, String, f64)]) -> String {
        let mut rows = rows.to_vec();
        rows.sort_by(|left, right| {
            (left.0, &left.1, &left.2)
                .cmp(&(right.0, &right.1, &right.2))
                .then(left.3.total_cmp(&right.3))
        });
        rows.iter()
            .map(|(timestamp, name, labels, value)| format!("{timestamp} {name} {labels} {value}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn ascending(rows: &[(u64, String, String, f64)]) -> bool {
        rows.windows(2).all(|pair| pair[0].0 <= pair[1].0)
    }

    /// Five flushed files that overlap in time and differ in which metrics
    /// they hold, one corrupt file, and an unflushed buffer: the shapes the
    /// statistics pruning has to get right. Timestamps never repeat, so
    /// every ordered answer has exactly one right order.
    async fn golden_store(store: &mut MayoStore, corrupt: Option<&Path>) {
        let series = [
            app_key("cpu", "default/web", "web-0"),
            app_key("cpu", "default/api", "api-0"),
            app_key("mem", "default/web", "web-0"),
            app_key("requests_total", "other/web", "web-0"),
        ];
        // (first timestamp, which series) per file; the last entry stays in
        // the buffer. Every first timestamp and step is a multiple of 50 and
        // each file owns its own slots below 50, so no two rows collide.
        let files: [(u64, &[usize]); 6] = [
            (10_000, &[0, 1, 2]),
            (10_300, &[0, 2]),
            (10_100, &[3]),
            (10_600, &[1, 3]),
            (10_450, &[0, 1, 2, 3]),
            (10_900, &[0, 3]),
        ];
        for (index, (first, which)) in files.iter().enumerate() {
            for step in 0..12 {
                for (position, series_index) in which.iter().enumerate() {
                    let slot = (index * 5 + position) as u64;
                    let timestamp = first + step * 50 + slot;
                    let value = (timestamp % 13) as f64 + *series_index as f64 / 4.0;
                    store.insert(&series[*series_index], Sample::at(timestamp, value));
                }
            }
            if index + 1 < files.len() {
                store.flush().await.unwrap();
            }
        }
        if let Some(directory) = corrupt {
            std::fs::write(directory.join("metrics_999999.parquet"), b"not parquet").unwrap();
        }
    }

    /// Every read the store offers, over the golden data, as one report.
    async fn golden_report(store: &MayoStore) -> String {
        let unbounded = i64::MAX as u64;
        let mut report = Vec::new();
        let mut ordered = |label: &str, rows: Vec<(u64, String, String, f64)>| {
            assert!(ascending(&rows), "{label} came back out of order");
            report.push(format!(
                "## {label} ({} rows)\n{}",
                rows.len(),
                canonical(&rows)
            ));
        };
        ordered(
            "query cpu unbounded",
            store.query("cpu", 0, unbounded).await.unwrap(),
        );
        ordered(
            "query cpu window",
            store.query("cpu", 10_200, 10_700).await.unwrap(),
        );
        ordered(
            "query mem one instant",
            store.query("mem", 10_002, 10_002).await.unwrap(),
        );
        ordered(
            "query missing",
            store.query("missing", 0, unbounded).await.unwrap(),
        );
        ordered(
            "query_all unbounded",
            store.query_all(0, unbounded).await.unwrap(),
        );
        ordered(
            "query_all window",
            store.query_all(10_250, 10_800).await.unwrap(),
        );
        ordered(
            "query_app web",
            store
                .query_app("default/web", None, 0, unbounded, None)
                .await
                .unwrap(),
        );
        ordered(
            "query_app web cpu newest two",
            store
                .query_app("default/web", Some("cpu"), 10_000, 11_000, Some(2))
                .await
                .unwrap(),
        );
        ordered(
            "query_names_since cpu mem",
            store
                .query_names_since(&["cpu", "mem"], 10_500)
                .await
                .unwrap(),
        );
        let alert = store
            .query_sql_since(
                "SELECT timestamp, metric_name, labels, value FROM metrics \
                 WHERE timestamp >= 10800 ORDER BY timestamp DESC",
                10_800,
            )
            .await
            .unwrap();
        assert!(alert.windows(2).all(|pair| pair[0].0 >= pair[1].0));
        report.push(format!(
            "## alert read ({} rows)\n{}",
            alert.len(),
            canonical(&alert)
        ));
        let unordered = store
            .query_sql("SELECT timestamp, metric_name, labels, value FROM metrics")
            .await
            .unwrap();
        report.push(format!(
            "## every row ({} rows)\n{}",
            unordered.len(),
            canonical(&unordered)
        ));

        let aggregates = store.query_window_aggregates(10_300, 10_900).await.unwrap();
        report.push(format!(
            "## window aggregates\n{}",
            aggregates
                .iter()
                .map(|row| format!("{row:?}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        report.push(format!(
            "## metric names\n{:?}",
            store.metric_names().await.unwrap()
        ));
        report.join("\n\n")
    }

    /// #377: streaming must return exactly what loading every file returned.
    /// The snapshot was recorded from the eager implementation before the
    /// change, and both backends must still match it.
    #[tokio::test]
    async fn every_read_matches_the_eager_golden_answers() {
        let (mut local, directory) = test_store();
        golden_store(&mut local, Some(directory.path())).await;
        let local_report = golden_report(&local).await;
        insta::assert_snapshot!("mayo_golden_reads", local_report);

        let bucket = tempfile::tempdir().unwrap();
        let url = url::Url::from_file_path(bucket.path()).unwrap().to_string();
        let mut remote = MayoStore::open(bucket.path().to_path_buf(), Some(&url))
            .await
            .unwrap();
        golden_store(&mut remote, Some(bucket.path())).await;
        assert_eq!(golden_report(&remote).await, local_report);
    }

    /// The eager reader this store used before #377: every file decoded in
    /// full into one in-memory table with the buffer, then queried.
    async fn eager_rows(store: &MayoStore, sql: &str) -> Vec<(u64, String, String, f64)> {
        use datafusion::datasource::MemTable;
        use datafusion::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

        let schema = Arc::new(metrics_schema());
        let mut batches = Vec::new();
        for entry in std::fs::read_dir(&store.data_dir).unwrap().flatten() {
            let path = entry.path();
            if !path.extension().is_some_and(|x| x == "parquet") {
                continue;
            }
            let Ok(builder) =
                ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&path).unwrap())
            else {
                continue;
            };
            for batch in builder.build().unwrap() {
                let batch = batch.unwrap();
                batches
                    .push(RecordBatch::try_new(schema.clone(), batch.columns().to_vec()).unwrap());
            }
        }
        batches.extend(store.buffer_to_batch().unwrap());
        batches.push(RecordBatch::new_empty(schema.clone()));
        let ctx = SessionContext::new();
        ctx.register_table(
            "metrics",
            Arc::new(MemTable::try_new(schema, vec![batches]).unwrap()),
        )
        .unwrap();
        let mut rows = Vec::new();
        for batch in ctx.sql(sql).await.unwrap().collect().await.unwrap() {
            let timestamps = batch
                .column(0)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap();
            let names = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            let labels = batch
                .column(2)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            let values = batch
                .column(3)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();
            for i in 0..batch.num_rows() {
                rows.push((
                    timestamps.value(i),
                    names.value(i).to_string(),
                    labels.value(i).to_string(),
                    values.value(i),
                ));
            }
        }
        rows
    }

    /// The query shapes the pruning has to survive: ranges, strict bounds,
    /// names, IN lists, disjunctions, negations and an aggregate.
    fn property_sql(shape: usize, a: u64, b: u64, name: &str, other: &str) -> String {
        let select = "SELECT timestamp, metric_name, labels, value FROM metrics";
        match shape {
            0 => format!("{select} WHERE timestamp >= {a} AND timestamp <= {b}"),
            1 => format!(
                "{select} WHERE metric_name = '{name}' AND timestamp > {a} AND timestamp < {b}"
            ),
            2 => format!(
                "{select} WHERE metric_name IN ('{name}', '{other}') \
                 AND timestamp BETWEEN {a} AND {b}"
            ),
            3 => format!("{select} WHERE timestamp < {a} OR metric_name = '{name}'"),
            4 => format!(
                "{select} WHERE (metric_name = '{name}' AND timestamp <= {a}) \
                 OR timestamp >= {b}"
            ),
            5 => format!("{select} WHERE NOT (timestamp >= {a} AND timestamp <= {b})"),
            6 => format!("{select} WHERE timestamp = {a} OR timestamp = {b}"),
            7 => format!("{select} WHERE timestamp > {a}"),
            8 => format!("{select} WHERE {b} > timestamp"),
            _ => format!(
                "SELECT MAX(timestamp) AS timestamp, metric_name, labels, \
                 SUM(value) AS value FROM metrics WHERE timestamp >= {a} \
                 GROUP BY metric_name, labels"
            ),
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

        /// #377: whatever the files hold and whatever the query asks, the
        /// streamed, pruned scan returns exactly the rows the eager reader
        /// did. Pruning may only skip data no answer could contain. Files are
        /// small and timestamps dense, so bounds often land exactly on a
        /// file's first or last sample, where an off-by-one would show.
        #[test]
        fn streamed_reads_match_the_eager_reader(
            files in proptest::collection::vec(
                proptest::collection::vec((0usize..4, 0u64..16, 0usize..2), 1..5),
                0..8,
            ),
            buffer in proptest::collection::vec((0usize..4, 0u64..16, 0usize..2), 0..4),
            shape in 0usize..10,
            a in 0u64..18,
            b in 0u64..18,
            name in 0usize..4,
            other in 0usize..4,
        ) {
            let names = ["alpha", "beta", "delta", "gamma"];
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let (mut store, _dir) = test_store();
                let insert = |store: &mut MayoStore, rows: &[(usize, u64, usize)]| {
                    for (index, (series, timestamp, app)) in rows.iter().enumerate() {
                        let key = app_key(names[*series], &format!("default/app-{app}"), "i-0");
                        store.insert(&key, Sample::at(*timestamp, index as f64));
                    }
                };
                for file in &files {
                    insert(&mut store, file);
                    store.flush().await.unwrap();
                }
                insert(&mut store, &buffer);

                let sql = property_sql(shape, a, b, names[name], names[other]);
                let streamed = store.query_sql(&sql).await.unwrap();
                let eager = eager_rows(&store, &sql).await;
                assert_eq!(canonical(&streamed), canonical(&eager), "{sql}");
                if shape == 0 {
                    let floored = store.query_sql_since(&sql, a).await.unwrap();
                    assert_eq!(canonical(&floored), canonical(&eager), "{sql} since {a}");
                }
            });
        }
    }

    /// A flushed metrics file is the only copy once the buffer is cleared,
    /// so a local object store must sync it.
    #[test]
    fn local_metrics_object_store_syncs_its_writes() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().to_str().unwrap().to_string();
        let url = format!("file://{bare}");
        for destination in [bare, url] {
            let (store, _) = parse_object_store(&destination).unwrap();
            let store = format!("{store:?}");
            assert!(store.contains("fsync: true"), "{destination}: {store}");
        }
    }
    #[tokio::test]
    async fn simultaneous_object_store_writers_keep_both_nodes_history() {
        let bucket = tempfile::tempdir().unwrap();
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let url = url::Url::from_file_path(bucket.path()).unwrap().to_string();
        let mut first = MayoStore::open(first_dir.path().to_path_buf(), Some(&url))
            .await
            .unwrap();
        let mut second = MayoStore::open(second_dir.path().to_path_buf(), Some(&url))
            .await
            .unwrap();
        let first_key = MetricKey::with_labels(
            "cpu",
            std::collections::BTreeMap::from([("node".into(), "a".into())]),
        );
        let second_key = MetricKey::with_labels(
            "cpu",
            std::collections::BTreeMap::from([("node".into(), "b".into())]),
        );
        first.insert(&first_key, Sample::at(100, 11.0));
        second.insert(&second_key, Sample::at(100, 22.0));
        let (a, b) = tokio::join!(first.flush(), second.flush());
        a.unwrap();
        b.unwrap();
        let rows = first.query("cpu", 0, 200).await.unwrap();
        assert_eq!(
            rows.len(),
            2,
            "one node overwrote the other node's first chunk"
        );
        assert!(rows.iter().any(|row| row.3 == 11.0));
        assert!(rows.iter().any(|row| row.3 == 22.0));
        first.insert(&first_key, Sample::at(101, 12.0));
        second.insert(&second_key, Sample::at(101, 23.0));
        let (a, b) = tokio::join!(first.flush(), second.flush());
        a.unwrap();
        b.unwrap();
        let restarted = MayoStore::open(first_dir.path().to_path_buf(), Some(&url))
            .await
            .unwrap();
        assert_eq!(restarted.query("cpu", 0, 200).await.unwrap().len(), 4);
        assert_eq!(std::fs::read_dir(bucket.path()).unwrap().count(), 4);
    }
    #[tokio::test]
    async fn node_archives_are_scoped_but_generic_queries_read_all_owners() {
        let bucket = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let url = url::Url::from_file_path(bucket.path()).unwrap().to_string();
        let mut one =
            MayoStore::open_for_node(local.path().join("one"), Some(&url), "cluster:a/node:a")
                .await
                .unwrap();
        let mut two =
            MayoStore::open_for_node(local.path().join("two"), Some(&url), "cluster:a/node:b")
                .await
                .unwrap();
        let key = MetricKey::simple("cpu");
        one.insert(&key, Sample::at(100, 11.0));
        two.insert(&key, Sample::at(100, 22.0));
        one.flush().await.unwrap();
        two.flush().await.unwrap();
        assert_eq!(
            one.query("cpu", 0, 200)
                .await
                .unwrap()
                .iter()
                .map(|row| row.3)
                .collect::<Vec<_>>(),
            vec![11.0]
        );
        assert_eq!(
            two.query("cpu", 0, 200)
                .await
                .unwrap()
                .iter()
                .map(|row| row.3)
                .collect::<Vec<_>>(),
            vec![22.0]
        );
        let recovered = MayoStore::open_for_node(
            local.path().join("lost-local-dir"),
            Some(&url),
            "cluster:a/node:a",
        )
        .await
        .unwrap();
        assert_eq!(
            recovered
                .query("cpu", 0, 200)
                .await
                .unwrap()
                .iter()
                .map(|row| row.3)
                .collect::<Vec<_>>(),
            vec![11.0]
        );
        let all = MayoStore::open(local.path().join("archive-reader"), Some(&url))
            .await
            .unwrap();
        assert_eq!(all.query("cpu", 0, 200).await.unwrap().len(), 2);
        assert!(
            MayoStore::open_for_node(local.path().join("empty-owner"), Some(&url), "")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn create_only_remote_chunks_refuse_collisions_without_replacing_history() {
        let bucket = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let url = url::Url::from_file_path(bucket.path()).unwrap().to_string();
        let mut store = MayoStore::open(local.path().to_path_buf(), Some(&url))
            .await
            .unwrap();
        let key = MetricKey::simple("cpu");
        store.insert(&key, Sample::at(100, 11.0));
        let pending = store.take_flush_batch().unwrap().unwrap();
        let retry_batch = pending.batch.clone();
        let FlushTarget::Remote {
            store: remote,
            key: location,
        } = &pending.target
        else {
            panic!("expected remote flush")
        };
        let mut earlier = MayoStore::new(local.path().join("earlier"));
        earlier.insert(&key, Sample::at(100, 99.0));
        let bytes = batch_to_parquet_bytes(&earlier.buffer_to_batch().unwrap().unwrap()).unwrap();
        remote
            .put_opts(
                location,
                object_store::PutPayload::from(bytes),
                object_store::PutOptions {
                    mode: object_store::PutMode::Create,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(write_pending_flush(pending).await.is_err());
        assert_eq!(
            store
                .query("cpu", 0, 200)
                .await
                .unwrap()
                .iter()
                .map(|row| row.3)
                .collect::<Vec<_>>(),
            vec![99.0]
        );
        // This is the rollback that both ordinary production flush APIs own.
        store.reabsorb_batch(&retry_batch);
        store.flush().await.unwrap();
        let mut values: Vec<_> = store
            .query("cpu", 0, 200)
            .await
            .unwrap()
            .iter()
            .map(|row| row.3)
            .collect();
        values.sort_by(f64::total_cmp);
        assert_eq!(values, vec![11.0, 99.0]);
    }
}
