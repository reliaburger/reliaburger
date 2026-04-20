//! Council-side rollup storage.
//!
//! Stores `NodeRollup` entries received from worker nodes. Uses the
//! same Arrow/DataFusion/Parquet architecture as `MayoStore`, but with
//! a different schema that includes node identity and aggregate columns
//! (min, max, sum, count) instead of a single value.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use datafusion::arrow::array::{Array, Float64Array, StringArray, UInt32Array, UInt64Array};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::MemTable;
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::prelude::*;

use super::rollup::NodeRollup;
use super::types::MayoError;

/// Arrow schema for the rollup table.
pub fn rollup_schema() -> Schema {
    Schema::new(vec![
        Field::new("timestamp", DataType::UInt64, false),
        Field::new("node_id", DataType::Utf8, false),
        Field::new("metric_name", DataType::Utf8, false),
        Field::new("labels", DataType::Utf8, false),
        Field::new("min_val", DataType::Float64, false),
        Field::new("max_val", DataType::Float64, false),
        Field::new("sum_val", DataType::Float64, false),
        Field::new("count_val", DataType::UInt32, false),
    ])
}

/// A buffered rollup entry waiting to be flushed.
struct BufferedRollup {
    timestamp: u64,
    node_id: String,
    metric_name: String,
    labels_json: String,
    min_val: f64,
    max_val: f64,
    sum_val: f64,
    count_val: u32,
}

/// Council-side storage for rollup data.
///
/// Each council member has one `RollupStore` that receives and stores
/// pre-aggregated `NodeRollup` entries from its assigned workers.
/// Cluster-wide queries are evaluated against this store rather than
/// fanning out to every worker node.
pub struct RollupStore {
    /// In-memory buffer of un-flushed rollup entries.
    buffer: Vec<BufferedRollup>,
    /// Accumulated RecordBatches (flushed from buffer).
    batches: Vec<RecordBatch>,
    /// Directory for Parquet files.
    data_dir: PathBuf,
    /// Counter for unique Parquet file names.
    flush_counter: u64,
}

impl RollupStore {
    /// Create a new store writing Parquet to `data_dir`.
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            buffer: Vec::new(),
            batches: Vec::new(),
            data_dir,
            flush_counter: 0,
        }
    }

    /// Ingest a `NodeRollup` into the buffer.
    pub fn ingest(&mut self, rollup: &NodeRollup) {
        let node_id = rollup.node_id.0.clone();
        for entry in &rollup.entries {
            let labels_json =
                serde_json::to_string(&entry.labels).unwrap_or_else(|_| "{}".to_string());
            self.buffer.push(BufferedRollup {
                timestamp: rollup.timestamp,
                node_id: node_id.clone(),
                metric_name: entry.metric_name.clone(),
                labels_json,
                min_val: entry.aggregate.min,
                max_val: entry.aggregate.max,
                sum_val: entry.aggregate.sum,
                count_val: entry.aggregate.count,
            });
        }
    }

    /// Number of un-flushed entries in the buffer.
    pub fn buffer_len(&self) -> usize {
        self.buffer.len()
    }

    /// Convert the buffer to an Arrow RecordBatch.
    fn buffer_to_batch(&self) -> Result<Option<RecordBatch>, MayoError> {
        if self.buffer.is_empty() {
            return Ok(None);
        }

        let timestamps: Vec<u64> = self.buffer.iter().map(|s| s.timestamp).collect();
        let node_ids: Vec<&str> = self.buffer.iter().map(|s| s.node_id.as_str()).collect();
        let names: Vec<&str> = self.buffer.iter().map(|s| s.metric_name.as_str()).collect();
        let labels: Vec<&str> = self.buffer.iter().map(|s| s.labels_json.as_str()).collect();
        let min_vals: Vec<f64> = self.buffer.iter().map(|s| s.min_val).collect();
        let max_vals: Vec<f64> = self.buffer.iter().map(|s| s.max_val).collect();
        let sum_vals: Vec<f64> = self.buffer.iter().map(|s| s.sum_val).collect();
        let count_vals: Vec<u32> = self.buffer.iter().map(|s| s.count_val).collect();

        let batch = RecordBatch::try_new(
            Arc::new(rollup_schema()),
            vec![
                Arc::new(UInt64Array::from(timestamps)),
                Arc::new(StringArray::from(node_ids)),
                Arc::new(StringArray::from(names)),
                Arc::new(StringArray::from(labels)),
                Arc::new(Float64Array::from(min_vals)),
                Arc::new(Float64Array::from(max_vals)),
                Arc::new(Float64Array::from(sum_vals)),
                Arc::new(UInt32Array::from(count_vals)),
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

        std::fs::create_dir_all(&self.data_dir).map_err(MayoError::Io)?;
        let filename = format!("rollup_{:06}.parquet", self.flush_counter);
        let path = self.data_dir.join(filename);

        let file = std::fs::File::create(&path).map_err(MayoError::Io)?;
        let mut writer = ArrowWriter::try_new(file, Arc::new(rollup_schema()), None)
            .map_err(|e| MayoError::Arrow(e.to_string()))?;
        writer
            .write(&batch)
            .map_err(|e| MayoError::Arrow(e.to_string()))?;
        writer
            .close()
            .map_err(|e| MayoError::Arrow(e.to_string()))?;

        self.batches.push(batch);
        self.buffer.clear();
        self.flush_counter += 1;
        Ok(())
    }

    /// Build a DataFusion session with all data (flushed + unflushed buffer).
    async fn session(&self) -> Result<SessionContext, MayoError> {
        let ctx = SessionContext::new();

        let mut all_batches = self.batches.clone();
        if let Some(buffer_batch) = self.buffer_to_batch()? {
            all_batches.push(buffer_batch);
        }

        if all_batches.is_empty() {
            let empty = RecordBatch::new_empty(Arc::new(rollup_schema()));
            let table = MemTable::try_new(Arc::new(rollup_schema()), vec![vec![empty]])
                .map_err(|e| MayoError::DataFusion(e.to_string()))?;
            ctx.register_table("rollups", Arc::new(table))
                .map_err(|e| MayoError::DataFusion(e.to_string()))?;
        } else {
            let table = MemTable::try_new(Arc::new(rollup_schema()), vec![all_batches])
                .map_err(|e| MayoError::DataFusion(e.to_string()))?;
            ctx.register_table("rollups", Arc::new(table))
                .map_err(|e| MayoError::DataFusion(e.to_string()))?;
        }

        Ok(ctx)
    }

    /// Query rollup data using SQL.
    ///
    /// Returns (timestamp, metric_name, labels, value) tuples where
    /// value is the sum across nodes for each (timestamp, metric_name, labels).
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

    /// Query cluster-wide aggregated metrics by name and time range.
    ///
    /// Returns (timestamp, metric_name, labels, sum) tuples aggregated
    /// across all nodes. For averages, callers can also retrieve count
    /// via `query_sql`.
    pub async fn query_cluster_metric(
        &self,
        metric_name: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        let sql = format!(
            "SELECT timestamp, metric_name, labels, SUM(sum_val) as total_sum \
             FROM rollups \
             WHERE metric_name = '{metric_name}' \
             AND timestamp >= {start} AND timestamp <= {end} \
             GROUP BY timestamp, metric_name, labels \
             ORDER BY timestamp"
        );
        self.query_sql(&sql).await
    }

    /// Query cluster-wide aggregated metrics with full aggregate info.
    ///
    /// Returns (timestamp, metric_name, labels, min, max, sum, count)
    /// aggregated across all nodes at each timestamp.
    pub async fn query_cluster_aggregates(
        &self,
        metric_name: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<ClusterAggregate>, MayoError> {
        let sql = format!(
            "SELECT timestamp, metric_name, labels, \
             MIN(min_val) as cluster_min, MAX(max_val) as cluster_max, \
             SUM(sum_val) as cluster_sum, SUM(count_val) as cluster_count \
             FROM rollups \
             WHERE metric_name = '{metric_name}' \
             AND timestamp >= {start} AND timestamp <= {end} \
             GROUP BY timestamp, metric_name, labels \
             ORDER BY timestamp"
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
            let mins = batch
                .column(3)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| MayoError::Arrow("min column type mismatch".into()))?;
            let maxs = batch
                .column(4)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| MayoError::Arrow("max column type mismatch".into()))?;
            let sums = batch
                .column(5)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| MayoError::Arrow("sum column type mismatch".into()))?;
            // SUM(count_val) returns u64 via DataFusion
            let counts = batch
                .column(6)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .ok_or_else(|| MayoError::Arrow("count column type mismatch".into()))?;

            for i in 0..batch.num_rows() {
                results.push(ClusterAggregate {
                    timestamp: timestamps.value(i),
                    metric_name: names.value(i).to_string(),
                    labels: labels.value(i).to_string(),
                    min: mins.value(i),
                    max: maxs.value(i),
                    sum: sums.value(i),
                    count: counts.value(i) as u32,
                });
            }
        }

        Ok(results)
    }

    /// List all distinct metric names in the rollup store.
    pub async fn metric_names(&self) -> Result<Vec<String>, MayoError> {
        let ctx = self.session().await?;
        let df = ctx
            .sql("SELECT DISTINCT metric_name FROM rollups ORDER BY metric_name")
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

/// A cluster-wide aggregate row with full statistics.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterAggregate {
    pub timestamp: u64,
    pub metric_name: String,
    pub labels: String,
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    pub count: u32,
}

impl ClusterAggregate {
    /// Compute the average value.
    pub fn avg(&self) -> Option<f64> {
        if self.count == 0 {
            None
        } else {
            Some(self.sum / self.count as f64)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::mayo::rollup::{NodeRollup, RollupAggregate, RollupEntry};
    use crate::meat::NodeId;

    fn test_store() -> (RollupStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = RollupStore::new(dir.path().to_path_buf());
        (store, dir)
    }

    fn make_rollup(node: &str, timestamp: u64, metric: &str, value: f64) -> NodeRollup {
        NodeRollup {
            node_id: NodeId::new(node),
            timestamp,
            entries: vec![RollupEntry {
                metric_name: metric.to_string(),
                labels: BTreeMap::new(),
                aggregate: RollupAggregate {
                    min: value,
                    max: value,
                    sum: value,
                    count: 1,
                },
            }],
        }
    }

    fn make_rollup_with_labels(
        node: &str,
        timestamp: u64,
        metric: &str,
        labels: BTreeMap<String, String>,
        min: f64,
        max: f64,
        sum: f64,
        count: u32,
    ) -> NodeRollup {
        NodeRollup {
            node_id: NodeId::new(node),
            timestamp,
            entries: vec![RollupEntry {
                metric_name: metric.to_string(),
                labels,
                aggregate: RollupAggregate {
                    min,
                    max,
                    sum,
                    count,
                },
            }],
        }
    }

    #[tokio::test]
    async fn ingest_and_flush_creates_parquet() {
        let (mut store, dir) = test_store();
        store.ingest(&make_rollup("n1", 1000, "cpu", 42.0));
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
        store.ingest(&make_rollup("n1", 1000, "cpu", 42.0));
        store.flush().await.unwrap();

        let results = store.query_cluster_metric("cpu", 0, 9999).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1000);
        assert_eq!(results[0].3, 42.0);
    }

    #[tokio::test]
    async fn query_unflushed_buffer_visible() {
        let (mut store, _dir) = test_store();
        store.ingest(&make_rollup("n1", 1000, "cpu", 42.0));
        // Don't flush — query should still see buffer data
        let results = store.query_cluster_metric("cpu", 0, 9999).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].3, 42.0);
    }

    #[tokio::test]
    async fn multiple_nodes_aggregated() {
        let (mut store, _dir) = test_store();
        store.ingest(&make_rollup("n1", 1000, "cpu", 10.0));
        store.ingest(&make_rollup("n2", 1000, "cpu", 20.0));
        store.ingest(&make_rollup("n3", 1000, "cpu", 30.0));
        store.flush().await.unwrap();

        let results = store.query_cluster_metric("cpu", 0, 9999).await.unwrap();
        assert_eq!(results.len(), 1);
        // SUM across nodes: 10 + 20 + 30 = 60
        assert_eq!(results[0].3, 60.0);
    }

    #[tokio::test]
    async fn cluster_aggregates_min_max_sum_count() {
        let (mut store, _dir) = test_store();
        store.ingest(&make_rollup("n1", 1000, "cpu", 10.0));
        store.ingest(&make_rollup("n2", 1000, "cpu", 20.0));
        store.ingest(&make_rollup("n3", 1000, "cpu", 30.0));
        store.flush().await.unwrap();

        let aggs = store
            .query_cluster_aggregates("cpu", 0, 9999)
            .await
            .unwrap();
        assert_eq!(aggs.len(), 1);
        assert_eq!(aggs[0].min, 10.0);
        assert_eq!(aggs[0].max, 30.0);
        assert_eq!(aggs[0].sum, 60.0);
        assert_eq!(aggs[0].count, 3);
        assert_eq!(aggs[0].avg(), Some(20.0));
    }

    #[tokio::test]
    async fn time_range_filtering() {
        let (mut store, _dir) = test_store();
        store.ingest(&make_rollup("n1", 100, "cpu", 1.0));
        store.ingest(&make_rollup("n1", 200, "cpu", 2.0));
        store.ingest(&make_rollup("n1", 300, "cpu", 3.0));
        store.flush().await.unwrap();

        let results = store.query_cluster_metric("cpu", 150, 250).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].3, 2.0);
    }

    #[tokio::test]
    async fn query_nonexistent_metric_returns_empty() {
        let (mut store, _dir) = test_store();
        store.ingest(&make_rollup("n1", 1000, "cpu", 42.0));
        store.flush().await.unwrap();

        let results = store
            .query_cluster_metric("nonexistent", 0, 9999)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn empty_store_returns_empty() {
        let (store, _dir) = test_store();
        let results = store
            .query_cluster_metric("anything", 0, 9999)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn flush_empty_buffer_is_noop() {
        let (mut store, _dir) = test_store();
        store.flush().await.unwrap();
    }

    #[tokio::test]
    async fn buffer_len_tracks_ingests() {
        let (mut store, _dir) = test_store();
        assert_eq!(store.buffer_len(), 0);
        store.ingest(&make_rollup("n1", 1000, "cpu", 42.0));
        assert_eq!(store.buffer_len(), 1);
    }

    #[tokio::test]
    async fn multiple_flushes_queryable() {
        let (mut store, _dir) = test_store();
        store.ingest(&make_rollup("n1", 1000, "cpu", 10.0));
        store.flush().await.unwrap();

        store.ingest(&make_rollup("n2", 1000, "cpu", 20.0));
        store.flush().await.unwrap();

        let results = store.query_cluster_metric("cpu", 0, 9999).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].3, 30.0);
    }

    #[tokio::test]
    async fn metric_names_lists_distinct() {
        let (mut store, _dir) = test_store();
        store.ingest(&make_rollup("n1", 1000, "beta", 1.0));
        store.ingest(&make_rollup("n1", 1000, "alpha", 2.0));
        store.ingest(&make_rollup("n2", 1000, "beta", 3.0));
        store.flush().await.unwrap();

        let names = store.metric_names().await.unwrap();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[tokio::test]
    async fn labels_preserved_in_rollup() {
        let mut labels = BTreeMap::new();
        labels.insert("app".to_string(), "web".to_string());
        let rollup = make_rollup_with_labels("n1", 1000, "cpu", labels, 10.0, 90.0, 300.0, 6);

        let (mut store, _dir) = test_store();
        store.ingest(&rollup);
        store.flush().await.unwrap();

        let results = store
            .query_sql(
                "SELECT timestamp, metric_name, labels, sum_val \
                 FROM rollups WHERE labels LIKE '%web%'",
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].2.contains("web"));
    }

    #[tokio::test]
    async fn schema_has_expected_columns() {
        let schema = rollup_schema();
        assert_eq!(schema.fields().len(), 8);
        assert_eq!(schema.field(0).name(), "timestamp");
        assert_eq!(schema.field(1).name(), "node_id");
        assert_eq!(schema.field(2).name(), "metric_name");
        assert_eq!(schema.field(3).name(), "labels");
        assert_eq!(schema.field(4).name(), "min_val");
        assert_eq!(schema.field(5).name(), "max_val");
        assert_eq!(schema.field(6).name(), "sum_val");
        assert_eq!(schema.field(7).name(), "count_val");
    }
}
