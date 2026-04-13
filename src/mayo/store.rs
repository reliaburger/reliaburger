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

/// Arrow schema for the metrics table.
pub fn metrics_schema() -> Schema {
    Schema::new(vec![
        Field::new("timestamp", DataType::UInt64, false),
        Field::new("metric_name", DataType::Utf8, false),
        Field::new("labels", DataType::Utf8, false),
        Field::new("value", DataType::Float64, false),
    ])
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
/// Inserts go into an in-memory buffer. On flush, the buffer is
/// converted to an Arrow RecordBatch and accumulated. All batches
/// are registered with DataFusion as a `MemTable` for SQL queries.
/// Parquet files are written for persistence.
pub struct MayoStore {
    /// In-memory buffer of unflushed samples.
    buffer: Vec<BufferedSample>,
    /// Accumulated RecordBatches (flushed from buffer).
    batches: Vec<RecordBatch>,
    /// Directory for Parquet files.
    data_dir: PathBuf,
    /// Counter for unique Parquet file names.
    flush_counter: u64,
}

impl MayoStore {
    /// Create a new store writing Parquet to `data_dir`.
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            buffer: Vec::new(),
            batches: Vec::new(),
            data_dir,
            flush_counter: 0,
        }
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

    /// Flush the buffer: convert to RecordBatch, write Parquet, accumulate.
    pub async fn flush(&mut self) -> Result<(), MayoError> {
        let batch = self.buffer_to_batch()?;
        let Some(batch) = batch else {
            return Ok(());
        };

        // Write Parquet file for persistence
        std::fs::create_dir_all(&self.data_dir).map_err(MayoError::Io)?;
        let filename = format!("metrics_{:06}.parquet", self.flush_counter);
        let path = self.data_dir.join(filename);

        let file = std::fs::File::create(&path).map_err(MayoError::Io)?;
        let mut writer = ArrowWriter::try_new(file, Arc::new(metrics_schema()), None)
            .map_err(|e| MayoError::Arrow(e.to_string()))?;
        writer
            .write(&batch)
            .map_err(|e| MayoError::Arrow(e.to_string()))?;
        writer
            .close()
            .map_err(|e| MayoError::Arrow(e.to_string()))?;

        // Accumulate in memory for queries
        self.batches.push(batch);
        self.buffer.clear();
        self.flush_counter += 1;
        Ok(())
    }

    /// Build a DataFusion session with all data (flushed + unflushed buffer).
    async fn session(&self) -> Result<SessionContext, MayoError> {
        let ctx = SessionContext::new();

        // Collect all batches: flushed + current buffer
        let mut all_batches = self.batches.clone();
        if let Some(buffer_batch) = self.buffer_to_batch()? {
            all_batches.push(buffer_batch);
        }

        if all_batches.is_empty() {
            let empty = RecordBatch::new_empty(Arc::new(metrics_schema()));
            let table = MemTable::try_new(Arc::new(metrics_schema()), vec![vec![empty]])
                .map_err(|e| MayoError::DataFusion(e.to_string()))?;
            ctx.register_table("metrics", Arc::new(table))
                .map_err(|e| MayoError::DataFusion(e.to_string()))?;
        } else {
            let table = MemTable::try_new(Arc::new(metrics_schema()), vec![all_batches])
                .map_err(|e| MayoError::DataFusion(e.to_string()))?;
            ctx.register_table("metrics", Arc::new(table))
                .map_err(|e| MayoError::DataFusion(e.to_string()))?;
        }
        Ok(ctx)
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
}
