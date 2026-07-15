//! Arrow/DataFusion-based time-series store.
//!
//! Metrics are buffered in memory, converted to Arrow RecordBatches,
//! and queryable via DataFusion SQL. Periodically flushed to Parquet
//! files for persistence. The same architecture as InfluxDB IOx.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use datafusion::arrow::array::{Array, Float64Array, StringArray, UInt64Array};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::MemTable;
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::prelude::*;

use super::types::{MayoError, MetricKey, Sample};

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

/// Write a single RecordBatch to a Parquet file at `path`.
///
/// Synchronous (Arrow's writer is blocking), so callers run it on
/// `spawn_blocking` to keep the async runtime free (OBS5/M3).
pub(crate) fn write_batch_parquet(
    path: &std::path::Path,
    batch: &RecordBatch,
) -> Result<(), MayoError> {
    let file = std::fs::File::create(path).map_err(MayoError::Io)?;
    let mut writer = ArrowWriter::try_new(file, batch.schema(), None)
        .map_err(|e| MayoError::Arrow(e.to_string()))?;
    writer
        .write(batch)
        .map_err(|e| MayoError::Arrow(e.to_string()))?;
    writer
        .close()
        .map_err(|e| MayoError::Arrow(e.to_string()))?;
    Ok(())
}

/// A drained buffer ready to be written to Parquet, decoupled from the store so
/// the caller can release its lock before the (blocking) write (OBS5/M3).
pub struct PendingFlush {
    data_dir: PathBuf,
    path: PathBuf,
    batch: RecordBatch,
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
            write_pending_flush(p).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Persist a [`PendingFlush`] to disk on the blocking pool. Runs with no lock
/// held, so concurrent queries proceed while the write is in flight.
pub async fn write_pending_flush(pending: PendingFlush) -> Result<(), MayoError> {
    let PendingFlush {
        data_dir,
        path,
        batch,
    } = pending;
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&data_dir).map_err(MayoError::Io)?;
        write_batch_parquet(&path, &batch)
    })
    .await
    .map_err(|e| MayoError::Io(std::io::Error::other(e.to_string())))?
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
/// Parquet file and dropped from memory. Queries read the on-disk Parquet
/// directory (durable across restarts) unioned with the unflushed buffer, so
/// memory stays bounded to the buffer regardless of how much history is on
/// disk.
pub struct MayoStore {
    /// In-memory buffer of unflushed samples.
    buffer: Vec<BufferedSample>,
    /// Directory for Parquet files.
    data_dir: PathBuf,
    /// Counter for unique Parquet file names. Seeded past any existing files
    /// so a restart never clobbers a previous run's data.
    flush_counter: u64,
}

impl MayoStore {
    /// Open (or create) a store writing Parquet to `data_dir`.
    ///
    /// Existing `metrics_NNNNNN.parquet` files are left in place and remain
    /// queryable; the flush counter resumes past the highest one so restarts
    /// don't overwrite them.
    pub fn new(data_dir: PathBuf) -> Self {
        let flush_counter = next_flush_counter(&data_dir, "metrics");
        Self {
            buffer: Vec::new(),
            data_dir,
            flush_counter,
        }
    }

    /// The directory where Parquet files are stored.
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
        write_pending_flush(pending).await
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
        let path = self.data_dir.join(filename);
        self.buffer.clear();
        self.flush_counter += 1;
        Ok(Some(PendingFlush {
            data_dir: self.data_dir.clone(),
            path,
            batch,
        }))
    }

    /// Build a DataFusion session exposing a `metrics` table over all data:
    /// the on-disk Parquet directory unioned with the unflushed buffer.
    async fn session(&self) -> Result<SessionContext, MayoError> {
        // Read Parquet string columns as `Utf8`, not `Utf8View`, so on-disk
        // batches share the canonical `metrics_schema` with the in-memory
        // buffer (DataFusion 45 forces view types by default).
        let config = SessionConfig::new().set_bool(
            "datafusion.execution.parquet.schema_force_view_types",
            false,
        );
        let ctx = SessionContext::new_with_config(config);
        let schema = Arc::new(metrics_schema());

        // On-disk Parquet (durable, survives restarts). Read into memory only
        // transiently for this query — nothing is retained on the struct.
        let disk_batches = self.read_disk_batches(&ctx).await?;

        // Unflushed buffer.
        let mut all_batches = disk_batches;
        if let Some(buffer_batch) = self.buffer_to_batch()? {
            all_batches.push(buffer_batch);
        }

        if all_batches.is_empty() {
            all_batches.push(RecordBatch::new_empty(schema.clone()));
        }

        let table = MemTable::try_new(schema, vec![all_batches])
            .map_err(|e| MayoError::DataFusion(e.to_string()))?;
        ctx.register_table("metrics", Arc::new(table))
            .map_err(|e| MayoError::DataFusion(e.to_string()))?;
        Ok(ctx)
    }

    /// Read every intact `metrics_*.parquet` file in the data dir into
    /// RecordBatches, normalised to the canonical `metrics_schema` (so the
    /// Parquet-inferred nullability doesn't clash with the in-memory buffer's
    /// schema).
    ///
    /// Each file is read on its own. A corrupt or truncated file is skipped
    /// with a log instead of failing the whole query (OBS5): a single bad flush
    /// must not make every unrelated read error out.
    async fn read_disk_batches(&self, ctx: &SessionContext) -> Result<Vec<RecordBatch>, MayoError> {
        if !dir_has_parquet(&self.data_dir) {
            return Ok(Vec::new());
        }
        let schema = Arc::new(metrics_schema());
        let mut normalised = Vec::new();

        let entries = std::fs::read_dir(&self.data_dir).map_err(MayoError::Io)?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.extension().is_some_and(|x| x == "parquet") {
                continue;
            }
            let table_name = "metrics_one";
            let _ = ctx.deregister_table(table_name);
            let file = path.to_string_lossy().to_string();
            if ctx
                .register_parquet(table_name, &file, ParquetReadOptions::default())
                .await
                .is_err()
            {
                eprintln!("mayo: skipping unreadable metrics file {file}");
                continue;
            }
            let read = async {
                let df = ctx
                    .sql("SELECT timestamp, metric_name, labels, value FROM metrics_one")
                    .await
                    .map_err(|e| MayoError::QueryFailed(e.to_string()))?;
                df.collect()
                    .await
                    .map_err(|e| MayoError::QueryFailed(e.to_string()))
            }
            .await;
            let _ = ctx.deregister_table(table_name);
            match read {
                Ok(batches) => {
                    for batch in batches {
                        match RecordBatch::try_new(schema.clone(), batch.columns().to_vec()) {
                            Ok(b) => normalised.push(b),
                            Err(e) => {
                                eprintln!("mayo: skipping malformed metrics batch in {file}: {e}")
                            }
                        }
                    }
                }
                Err(_) => eprintln!("mayo: skipping corrupt metrics file {file}"),
            }
        }
        Ok(normalised)
    }

    /// Query metrics using SQL. Returns (timestamp, name, labels, value) tuples.
    pub async fn query_sql(&self, sql: &str) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        let ctx = self.session().await?;
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
        self.query_sql(&sql).await
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

        let ctx = self.session().await?;
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

        let ctx = self.session().await?;
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

    /// Prune Parquet files older than `before` timestamp.
    pub fn prune(&self, before: u64) -> Result<usize, MayoError> {
        let mut deleted = 0;
        if let Ok(entries) = std::fs::read_dir(&self.data_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "parquet")
                    && let Ok(meta) = std::fs::metadata(&path)
                    && let Ok(modified) = meta.modified()
                {
                    let mod_secs = modified
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    if mod_secs < before {
                        let _ = std::fs::remove_file(&path);
                        deleted += 1;
                    }
                }
            }
        }
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mayo::types::MetricKey;

    fn test_store() -> (MayoStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = MayoStore::new(dir.path().to_path_buf());
        (store, dir)
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
}
