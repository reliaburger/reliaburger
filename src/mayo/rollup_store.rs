//! Council-side rollup storage.
//!
//! Stores `NodeRollup` entries received from worker nodes. Uses the
//! same Arrow/DataFusion/Parquet architecture as `MayoStore`, but with
//! a different schema that includes node identity and aggregate columns
//! (min, max, sum, count) instead of a single value.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use datafusion::arrow::array::{Array, Float64Array, StringArray, UInt32Array, UInt64Array};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::MemTable;
use datafusion::prelude::*;

use super::rollup::NodeRollup;
use super::store::{dir_has_parquet, next_flush_counter, write_batch_parquet};
use super::types::MayoError;

/// Cap on un-flushed buffered rollup rows. A council member receives rollups
/// from many workers; without a ceiling a flush that keeps failing (full disk,
/// permissions) would let the buffer grow without bound and OOM the process
/// (OBS2). When the cap is hit the oldest rows are dropped, trading the oldest
/// un-persisted data for a bounded footprint.
const MAX_BUFFER_ROWS: usize = 1_000_000;

/// How far back the idempotency tracker remembers `(node, window)` keys. A
/// worker only re-sends recent windows as reassignment backfill (the extended
/// window is 5 minutes), so keys older than this can never arrive again and are
/// safe to forget. Bounding the horizon keeps the tracker from growing without
/// bound over a long-running process (OBS2). One hour is comfortably past any
/// backfill.
const IDEMPOTENCY_HORIZON_SECS: u64 = 3600;

/// Prune the idempotency tracker once it exceeds this many keys. A generous
/// ceiling: with hundreds of nodes on a 60s window an hour is a few thousand
/// keys, so pruning is rare and ingest stays O(1) amortised.
const MAX_SEEN_WINDOWS: usize = 100_000;

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
    /// Directory for Parquet files.
    data_dir: PathBuf,
    /// Counter for unique Parquet file names. Seeded past any existing files so
    /// a restart never clobbers a previous run's `rollup_NNNNNN.parquet`.
    flush_counter: u64,
    /// `(node_id, window)` keys ingested within the recent backfill horizon. A
    /// worker re-sends the same window after reassignment (extended backfill),
    /// so the aggregator must ingest each `(node, window)` exactly once or the
    /// cluster sum double-counts (OBS2). Pruned to stay bounded.
    seen_windows: HashSet<(String, u64)>,
    /// Newest window timestamp seen, used as the anchor for pruning
    /// `seen_windows` (kept off the wall clock for determinism).
    newest_window: u64,
}

impl RollupStore {
    /// Open (or create) a store writing Parquet to `data_dir`.
    ///
    /// Existing `rollup_NNNNNN.parquet` files stay in place and remain
    /// queryable; the flush counter resumes past the highest one so a restart
    /// appends rather than overwriting.
    pub fn new(data_dir: PathBuf) -> Self {
        let flush_counter = next_flush_counter(&data_dir, "rollup");
        Self {
            buffer: Vec::new(),
            data_dir,
            flush_counter,
            seen_windows: HashSet::new(),
            newest_window: 0,
        }
    }

    /// Ingest a `NodeRollup` into the buffer.
    ///
    /// Idempotent per `(node_id, window)`: a window already ingested this
    /// lifetime is dropped, so a re-sent rollup (worker reassignment backfill)
    /// never double-counts. Returns `true` if the rollup was ingested, `false`
    /// if it was a duplicate.
    pub fn ingest(&mut self, rollup: &NodeRollup) -> bool {
        let node_id = rollup.node_id.0.clone();
        let key = (node_id.clone(), rollup.timestamp);
        if !self.seen_windows.insert(key) {
            return false;
        }

        // Forget keys older than the backfill horizon so the tracker stays
        // bounded. Pruned only occasionally (when the set grows large) to keep
        // ingest O(1) amortised, and anchored on the newest window seen — not
        // the wall clock — so it's deterministic and works with the historical
        // timestamps used in tests.
        self.newest_window = self.newest_window.max(rollup.timestamp);
        if self.seen_windows.len() > MAX_SEEN_WINDOWS {
            let cutoff = self.newest_window.saturating_sub(IDEMPOTENCY_HORIZON_SECS);
            self.seen_windows.retain(|(_, window)| *window >= cutoff);
        }

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

        // Bound the buffer: drop the oldest rows if a stuck flush let it grow
        // past the cap.
        if self.buffer.len() > MAX_BUFFER_ROWS {
            let overflow = self.buffer.len() - MAX_BUFFER_ROWS;
            self.buffer.drain(0..overflow);
        }
        true
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

    /// Flush the buffer: convert to a RecordBatch and persist it to Parquet.
    ///
    /// The batch is written on a blocking thread (`spawn_blocking`) so the
    /// synchronous Arrow/Parquet encode never stalls the async runtime (OBS5).
    /// Once on disk the rows are dropped from memory; queries read them back
    /// from the Parquet directory, so a restart recovers all history and
    /// memory stays bounded to the buffer.
    pub async fn flush(&mut self) -> Result<(), MayoError> {
        let batch = self.buffer_to_batch()?;
        let Some(batch) = batch else {
            return Ok(());
        };

        std::fs::create_dir_all(&self.data_dir).map_err(MayoError::Io)?;
        let filename = format!("rollup_{:06}.parquet", self.flush_counter);
        let path = self.data_dir.join(filename);

        // Move the write off the async runtime. `spawn_blocking` runs the
        // closure on tokio's blocking pool and hands back its result; the
        // outer `?` on `.await` surfaces a join error, the inner one the
        // write error.
        tokio::task::spawn_blocking(move || write_batch_parquet(&path, &batch))
            .await
            .map_err(|e| MayoError::Io(std::io::Error::other(e.to_string())))??;

        self.buffer.clear();
        self.flush_counter += 1;
        Ok(())
    }

    /// Build a DataFusion session exposing a `rollups` table over all data: the
    /// on-disk Parquet directory unioned with the unflushed buffer.
    async fn session(&self) -> Result<SessionContext, MayoError> {
        // Read Parquet string columns as `Utf8`, not `Utf8View`, so on-disk
        // batches share the canonical schema with the in-memory buffer.
        let config = SessionConfig::new().set_bool(
            "datafusion.execution.parquet.schema_force_view_types",
            false,
        );
        let ctx = SessionContext::new_with_config(config);
        let schema = Arc::new(rollup_schema());

        let mut all_batches = self.read_disk_batches(&ctx).await?;
        if let Some(buffer_batch) = self.buffer_to_batch()? {
            all_batches.push(buffer_batch);
        }

        if all_batches.is_empty() {
            all_batches.push(RecordBatch::new_empty(schema.clone()));
        }

        let table = MemTable::try_new(schema, vec![all_batches])
            .map_err(|e| MayoError::DataFusion(e.to_string()))?;
        ctx.register_table("rollups", Arc::new(table))
            .map_err(|e| MayoError::DataFusion(e.to_string()))?;
        Ok(ctx)
    }

    /// Read every intact `rollup_*.parquet` file into RecordBatches, normalised
    /// to the canonical schema. A corrupt or truncated file is skipped with a
    /// log rather than failing the whole query (OBS5): one bad flush must not
    /// take down every unrelated read.
    async fn read_disk_batches(&self, ctx: &SessionContext) -> Result<Vec<RecordBatch>, MayoError> {
        if !dir_has_parquet(&self.data_dir) {
            return Ok(Vec::new());
        }
        let schema = Arc::new(rollup_schema());
        let mut normalised = Vec::new();

        let entries = std::fs::read_dir(&self.data_dir).map_err(MayoError::Io)?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.extension().is_some_and(|x| x == "parquet") {
                continue;
            }
            // Register and read each file on its own so a single corrupt file
            // can be skipped instead of poisoning a directory-wide scan.
            let table_name = "rollup_one";
            let _ = ctx.deregister_table(table_name);
            let file = path.to_string_lossy().to_string();
            if ctx
                .register_parquet(table_name, &file, ParquetReadOptions::default())
                .await
                .is_err()
            {
                eprintln!("mayo: skipping unreadable rollup file {file}");
                continue;
            }
            let read = async {
                let df = ctx
                    .sql("SELECT timestamp, node_id, metric_name, labels, min_val, max_val, sum_val, count_val FROM rollup_one")
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
                                eprintln!("mayo: skipping malformed rollup batch in {file}: {e}")
                            }
                        }
                    }
                }
                Err(_) => eprintln!("mayo: skipping corrupt rollup file {file}"),
            }
        }
        Ok(normalised)
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
        // Escape the caller-supplied name: it reaches here from `/v1/metrics/rollup`
        // and is interpolated into SQL. Without this a crafted name could drop the
        // time/name predicates (OBS1).
        let metric_name = crate::mayo::store::escape_sql_literal(metric_name);
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
        // Escape the caller-supplied name before interpolation (OBS1).
        let metric_name = crate::mayo::store::escape_sql_literal(metric_name);
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

    #[allow(clippy::too_many_arguments)]
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
    async fn cluster_metric_query_neutralises_sql_injection() {
        // OBS1: a crafted metric name must not drop the predicate and leak
        // every metric. With escaping, the injected string is looked up
        // literally (matching nothing), not executed as SQL.
        let (mut store, _dir) = test_store();
        // One node reports both metrics for the same window in a single rollup.
        let mut rollup = make_rollup("n1", 1000, "cpu", 10.0);
        rollup.entries.push(
            make_rollup("n1", 1000, "secret_metric", 99.0)
                .entries
                .remove(0),
        );
        store.ingest(&rollup);
        store.flush().await.unwrap();

        let injected = store
            .query_cluster_metric("x' OR '1'='1", 0, 9999)
            .await
            .unwrap();
        assert!(
            injected.is_empty(),
            "injection must match nothing, got {injected:?}"
        );

        // The benign path still works.
        let ok = store.query_cluster_metric("cpu", 0, 9999).await.unwrap();
        assert_eq!(ok.len(), 1);
    }

    #[tokio::test]
    async fn metric_names_lists_distinct() {
        let (mut store, _dir) = test_store();
        // n1's window carries both metrics in one rollup; n2 carries beta.
        let mut n1 = make_rollup("n1", 1000, "beta", 1.0);
        n1.entries
            .push(make_rollup("n1", 1000, "alpha", 2.0).entries.remove(0));
        store.ingest(&n1);
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

    #[tokio::test]
    async fn resent_window_does_not_double_count() {
        // OBS2: a worker re-sends the same (node, window) after reassignment
        // (extended backfill). The council must ingest it exactly once, or the
        // cluster sum doubles.
        let (mut store, _dir) = test_store();
        let rollup = make_rollup("n1", 1000, "cpu", 42.0);

        let first = store.ingest(&rollup);
        let second = store.ingest(&rollup); // exact resend
        assert!(first, "first ingest should be accepted");
        assert!(!second, "duplicate (node, window) must be dropped");

        store.flush().await.unwrap();
        let results = store.query_cluster_metric("cpu", 0, 9999).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].3, 42.0, "value must not double to 84");
    }

    #[tokio::test]
    async fn distinct_windows_from_same_node_both_ingest() {
        // Idempotency keys on (node, window), so a genuinely later window from
        // the same node is not a duplicate.
        let (mut store, _dir) = test_store();
        assert!(store.ingest(&make_rollup("n1", 1000, "cpu", 10.0)));
        assert!(store.ingest(&make_rollup("n1", 1060, "cpu", 20.0)));
        assert_eq!(store.buffer_len(), 2);
    }

    #[tokio::test]
    async fn restart_resumes_flush_counter_without_clobbering() {
        // OBS2: `flush_counter` was hardcoded to 0, so a restart overwrote
        // `rollup_000000.parquet`. It must resume past the highest existing
        // file, and prior data must still be queryable.
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = RollupStore::new(dir.path().to_path_buf());
            store.ingest(&make_rollup("n1", 1000, "cpu", 10.0));
            store.flush().await.unwrap();
            store.ingest(&make_rollup("n2", 1000, "cpu", 20.0));
            store.flush().await.unwrap();
        }
        let count_parquet = |d: &std::path::Path| {
            std::fs::read_dir(d)
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .path()
                        .extension()
                        .is_some_and(|x| x == "parquet")
                })
                .count()
        };
        assert_eq!(count_parquet(dir.path()), 2);

        // Restart over the same dir.
        let mut store = RollupStore::new(dir.path().to_path_buf());
        let prior = store.query_cluster_metric("cpu", 0, 9999).await.unwrap();
        assert_eq!(prior.len(), 1, "persisted rollups not reloaded on restart");
        assert_eq!(prior[0].3, 30.0);

        store.ingest(&make_rollup("n3", 1000, "cpu", 30.0));
        store.flush().await.unwrap();
        assert_eq!(
            count_parquet(dir.path()),
            3,
            "restart clobbered an existing rollup file"
        );
    }

    #[tokio::test]
    async fn buffer_is_bounded() {
        // OBS2: the buffer must not grow without bound if flushing stalls.
        let (mut store, _dir) = test_store();
        // Ingest more distinct windows than the cap; each carries one entry.
        let over = MAX_BUFFER_ROWS + 100;
        for i in 0..over {
            store.ingest(&make_rollup("n1", i as u64, "cpu", 1.0));
        }
        assert!(
            store.buffer_len() <= MAX_BUFFER_ROWS,
            "buffer grew past the cap: {}",
            store.buffer_len()
        );
    }

    #[tokio::test]
    async fn corrupt_parquet_file_does_not_fail_query() {
        // OBS5: a truncated/garbage Parquet file in the data dir must be
        // skipped, not fail an unrelated query.
        let (mut store, dir) = test_store();
        store.ingest(&make_rollup("n1", 1000, "cpu", 10.0));
        store.flush().await.unwrap();

        // Drop a corrupt file alongside the good one.
        std::fs::write(dir.path().join("rollup_999999.parquet"), b"not parquet").unwrap();

        let results = store.query_cluster_metric("cpu", 0, 9999).await.unwrap();
        assert_eq!(results.len(), 1, "corrupt file broke an unrelated query");
        assert_eq!(results[0].3, 10.0);
    }
}
