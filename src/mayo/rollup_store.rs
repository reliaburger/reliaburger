//! Council-side rollup storage.
//!
//! Stores `NodeRollup` entries received from worker nodes. Uses the
//! same Arrow/DataFusion/Parquet architecture as `MayoStore`, but with
//! a different schema that includes node identity and aggregate columns
//! (min, max, sum, count) instead of a single value.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use datafusion::arrow::array::{Array, Float64Array, StringArray, UInt32Array, UInt64Array};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::*;

use super::rollup::{MetricsQueryRow, NodeRollup, OwnedRollupRow, RollupReporter};
use super::scan::{
    ParquetSource, ParquetTable, SourceIndex, batch_time_range, list_local, list_local_off_runtime,
    streaming_session,
};
use super::store::{
    ALL_QUERY_ROW_LIMIT, dir_has_parquet_off_runtime, next_flush_counter, write_batch_parquet,
};
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

/// The newest window any rollup file in `directory` holds, from footer
/// statistics alone. A file whose footer can't be read is skipped, as every
/// query skips it. Blocking: it opens every file.
fn newest_flushed_window(directory: &std::path::Path) -> Option<u64> {
    list_local(directory)
        .ok()?
        .iter()
        .filter_map(|source| match source {
            ParquetSource::Local(path) => super::store::file_max_timestamp(path),
            ParquetSource::Remote { .. } => None,
        })
        .max()
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
    /// In-memory buffer of un-flushed rollup entries, oldest first. A ring
    /// buffer, so shedding the oldest moves nothing (#2450).
    buffer: VecDeque<BufferedRollup>,
    /// Directory for Parquet files.
    data_dir: PathBuf,
    /// Counter for unique Parquet file names. Seeded past any existing files so
    /// a restart never clobbers a previous run's `rollup_NNNNNN.parquet`.
    flush_counter: u64,
    /// `(node_id, window)` keys ingested within the recent backfill horizon. A
    /// worker re-sends the same window after reassignment (extended backfill),
    /// so the aggregator must ingest each `(node, window)` exactly once or the
    /// cluster sum double-counts (OBS2). Pruned to stay bounded.
    seen_windows: HashSet<(String, u64, u16)>,
    /// `(node_id, window)` keys found flushed on disk at startup. The files
    /// don't record which part a row came from, so any part of such a
    /// window is taken as already ingested: a part straddling a restart is
    /// lost rather than counted twice.
    hydrated_windows: HashSet<(String, u64)>,
    /// Newest window timestamp seen, used as the anchor for pruning
    /// `seen_windows` (kept off the wall clock for determinism).
    newest_window: u64,
    /// The newest window each node has sent, duplicates included: a cluster
    /// query names a node that stopped reporting (#2536). One entry per
    /// node that ever reported.
    last_windows: HashMap<String, u64>,
    /// Each Parquet file's time range, so a query skips the files outside
    /// its window without reading their footers (#2425).
    index: SourceIndex,
    /// The most unflushed rows kept: [`MAX_BUFFER_ROWS`], smaller in tests.
    max_buffer_rows: usize,
    /// Rows frozen for a flush being written outside the store lock, kept
    /// until their file is written (#2443).
    pending: Option<RollupPendingFlush>,
    /// Parks each query of a snapshot this store hands out, so a test can
    /// check what a caller holds while its query runs (#2530).
    #[cfg(test)]
    query_gate: Option<Arc<super::store::QueryGate>>,
}

/// A batch of rollups frozen for one flush, written with
/// [`write_rollup_pending`] while the store goes on taking rollups.
#[derive(Clone)]
pub struct RollupPendingFlush {
    data_dir: PathBuf,
    path: PathBuf,
    batch: RecordBatch,
}

/// Write `pending`'s Parquet file. Needs no store lock: the store keeps the
/// rows until [`RollupStore::complete_flush`], and a failed write leaves
/// them to be retried.
pub async fn write_rollup_pending(pending: &RollupPendingFlush) -> Result<(), MayoError> {
    tokio::fs::create_dir_all(&pending.data_dir)
        .await
        .map_err(MayoError::Io)?;
    let path = pending.path.clone();
    let batch = pending.batch.clone();
    // Move the write off the async runtime. `spawn_blocking` runs the
    // closure on tokio's blocking pool and hands back its result; the outer
    // `?` on `.await` surfaces a join error, the inner one the write error.
    tokio::task::spawn_blocking(move || write_batch_parquet(&path, &batch))
        .await
        .map_err(|e| MayoError::Io(std::io::Error::other(e.to_string())))?
}

/// Flush a shared store without holding its lock during the write: the
/// lock is taken only to freeze the batch and to drop it once written, so
/// the aggregator's ingest and every query go on meanwhile (#2443). At
/// most two batches: a retried one, then the current buffer.
pub async fn flush_off_lock(store: &tokio::sync::RwLock<RollupStore>) -> Result<(), MayoError> {
    for _ in 0..2 {
        let pending = store.write().await.take_flush_batch()?;
        let Some(pending) = pending else {
            break;
        };
        write_rollup_pending(&pending).await?;
        store.write().await.complete_flush(&pending);
    }
    Ok(())
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
            buffer: VecDeque::new(),
            data_dir,
            flush_counter,
            seen_windows: HashSet::new(),
            hydrated_windows: HashSet::new(),
            newest_window: 0,
            last_windows: HashMap::new(),
            index: SourceIndex::default(),
            max_buffer_rows: MAX_BUFFER_ROWS,
            pending: None,
            #[cfg(test)]
            query_gate: None,
        }
    }

    /// Park every query of this store's later snapshots at `gate`.
    #[cfg(test)]
    pub(crate) fn set_query_gate(&mut self, gate: Arc<super::store::QueryGate>) {
        self.query_gate = Some(gate);
    }

    /// Seed the idempotency tracker from Parquet already on disk (M17).
    ///
    /// `seen_windows` is process-local and starts empty after a restart, while
    /// the flushed Parquet already holds prior windows. A reassigned worker
    /// re-sending a `(node, window)` that was flushed before the restart would
    /// then be ingested a second time, doubling `query_cluster_metric` sums for
    /// that window. Loading the distinct `(node_id, timestamp)` pairs within the
    /// idempotency horizon once at startup closes that gap. Call after `new()`
    /// and before the first ingest.
    pub async fn hydrate_seen_windows(&mut self) -> Result<(), MayoError> {
        if !dir_has_parquet_off_runtime(&self.data_dir).await {
            return Ok(());
        }
        // Only windows within the horizon of the newest one are kept, so only
        // they are read, with the newest taken from the files' footers.
        let directory = self.data_dir.clone();
        let cutoff = tokio::task::spawn_blocking(move || newest_flushed_window(&directory))
            .await
            .ok()
            .flatten()
            .map_or(0, |newest| newest.saturating_sub(IDEMPOTENCY_HORIZON_SECS));
        let ctx = self.session().await?;
        // Startup work, not a caller's query: no deadline.
        let batches = ctx
            .sql(&format!(
                "SELECT DISTINCT node_id, timestamp FROM rollups WHERE timestamp >= {cutoff}"
            ))
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?
            .collect()
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;
        for batch in &batches {
            if batch.num_columns() < 2 {
                continue;
            }
            let nodes = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| MayoError::Arrow("node_id column type mismatch".into()))?;
            let windows = batch
                .column(1)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .ok_or_else(|| MayoError::Arrow("timestamp column type mismatch".into()))?;
            for i in 0..batch.num_rows() {
                let window = windows.value(i);
                self.newest_window = self.newest_window.max(window);
                self.note_window(nodes.value(i), window);
                self.hydrated_windows
                    .insert((nodes.value(i).to_string(), window));
            }
        }
        // Keep the seeded set bounded to the idempotency horizon.
        let cutoff = self.newest_window.saturating_sub(IDEMPOTENCY_HORIZON_SECS);
        self.hydrated_windows
            .retain(|(_, window)| *window >= cutoff);
        Ok(())
    }

    /// Ingest a `NodeRollup` into the buffer.
    ///
    /// Idempotent per `(node_id, window, part)`: a part already ingested this
    /// lifetime is dropped, so a re-sent rollup (worker reassignment backfill)
    /// never double-counts. Returns `true` if the rollup was ingested, `false`
    /// if it was a duplicate.
    pub fn ingest(&mut self, rollup: &NodeRollup) -> bool {
        let node_id = rollup.node_id.0.clone();
        // A re-sent window still shows the node is reporting.
        self.note_window(&node_id, rollup.timestamp);
        if self
            .hydrated_windows
            .contains(&(node_id.clone(), rollup.timestamp))
        {
            return false;
        }
        let key = (node_id.clone(), rollup.timestamp, rollup.part);
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
            self.seen_windows.retain(|(_, window, _)| *window >= cutoff);
            self.hydrated_windows
                .retain(|(_, window)| *window >= cutoff);
        }

        for entry in &rollup.entries {
            let labels_json =
                serde_json::to_string(&entry.labels).unwrap_or_else(|_| "{}".to_string());
            self.buffer.push_back(BufferedRollup {
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
        let reserved = self
            .pending
            .as_ref()
            .map_or(0, |pending| pending.batch.num_rows());
        let available = self.max_buffer_rows.saturating_sub(reserved);
        while self.buffer.len() > available && self.buffer.pop_front().is_some() {}
        true
    }

    /// The newest window this aggregator holds from each node that has
    /// reported to it, flushed or not (#2536).
    pub fn reporters(&self) -> Vec<RollupReporter> {
        self.last_windows
            .iter()
            .map(|(node_id, last_window)| RollupReporter {
                node_id: node_id.clone(),
                last_window: *last_window,
            })
            .collect()
    }

    /// Record that `node_id` sent the window starting at `window`.
    fn note_window(&mut self, node_id: &str, window: u64) {
        match self.last_windows.get_mut(node_id) {
            Some(last) => *last = (*last).max(window),
            None => {
                self.last_windows.insert(node_id.to_string(), window);
            }
        }
    }

    /// Number of un-flushed entries in the buffer.
    pub fn buffer_len(&self) -> usize {
        self.buffer.len()
            + self
                .pending
                .as_ref()
                .map_or(0, |pending| pending.batch.num_rows())
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

    /// Flush the buffer to Parquet, holding the store throughout: at most
    /// a retried pending batch, then the current buffer. Callers that share
    /// the store behind a lock use [`flush_off_lock`] instead, so queries and
    /// ingest go on during the write (#2443).
    pub async fn flush(&mut self) -> Result<(), MayoError> {
        for _ in 0..2 {
            let Some(pending) = self.take_flush_batch()? else {
                break;
            };
            write_rollup_pending(&pending).await?;
            self.complete_flush(&pending);
        }
        Ok(())
    }

    /// Freeze the buffer into a batch for a flush written outside the store
    /// lock, or hand back the batch an earlier flush failed to write, so it
    /// is retried before newer rows. The rows stay queryable, from memory,
    /// until [`complete_flush`](Self::complete_flush). `None` when there is
    /// nothing to write.
    pub fn take_flush_batch(&mut self) -> Result<Option<RollupPendingFlush>, MayoError> {
        if let Some(pending) = &self.pending {
            return Ok(Some(pending.clone()));
        }
        let Some(batch) = self.buffer_to_batch()? else {
            return Ok(None);
        };
        let filename = format!("rollup_{:06}.parquet", self.flush_counter);
        let path = self.data_dir.join(filename);
        if let Some(range) = batch_time_range(&batch) {
            self.index.record(path.display().to_string(), range);
        }
        self.flush_counter += 1;
        self.buffer.clear();
        let pending = RollupPendingFlush {
            data_dir: self.data_dir.clone(),
            path,
            batch,
        };
        self.pending = Some(pending.clone());
        Ok(Some(pending))
    }

    /// Forget `flushed` once its file is written: queries read it from disk
    /// from now on.
    pub fn complete_flush(&mut self, flushed: &RollupPendingFlush) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.path == flushed.path)
        {
            self.pending = None;
        }
    }

    /// What one query reads, captured now: the Parquet files and the
    /// unflushed rollups. Query it after releasing the store's lock, so a
    /// slow query never holds up the aggregator's ingest or a flush (#2443,
    /// #2449):
    ///
    /// ```ignore
    /// let snapshot = rollups.read().await.snapshot().await?;
    /// let rows = snapshot.query_cluster_metric("cpu", start, end).await?;
    /// ```
    ///
    /// The rollups table is streamed per query and pruned by each query's
    /// time and metric-name predicates (#377), so a cluster query reads the
    /// files its window touches, not every one.
    pub async fn snapshot(&self) -> Result<RollupSnapshot, MayoError> {
        let mut sources = list_local_off_runtime(&self.data_dir).await?;
        let mut memory: Vec<RecordBatch> = self.buffer_to_batch()?.into_iter().collect();
        if let Some(pending) = &self.pending {
            // A pending batch is read from memory until its flush completes,
            // even if its file has just appeared, so no row is read twice.
            sources.retain(
                |source| !matches!(source, ParquetSource::Local(path) if *path == pending.path),
            );
            memory.push(pending.batch.clone());
        }
        let memory = match memory.len() {
            0 => None,
            1 => memory.pop(),
            _ => Some(
                datafusion::arrow::compute::concat_batches(&Arc::new(rollup_schema()), &memory)
                    .map_err(|error| MayoError::Arrow(error.to_string()))?,
            ),
        };
        Ok(RollupSnapshot {
            sources: Arc::new(sources),
            memory,
            index: self.index.clone(),
            #[cfg(test)]
            query_gate: self.query_gate.clone(),
        })
    }

    async fn session(&self) -> Result<SessionContext, MayoError> {
        self.snapshot().await?.session()
    }

    /// See [`RollupSnapshot::query_sql`].
    pub async fn query_sql(&self, sql: &str) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        self.snapshot().await?.query_sql(sql).await
    }

    /// See [`RollupSnapshot::query_owned_rows`].
    pub async fn query_owned_rows(
        &self,
        metric_name: Option<&str>,
        start: u64,
        end: u64,
    ) -> Result<Vec<OwnedRollupRow>, MayoError> {
        self.snapshot()
            .await?
            .query_owned_rows(metric_name, start, end)
            .await
    }

    /// See [`RollupSnapshot::query_owned_page`].
    pub async fn query_owned_page(
        &self,
        metric_name: Option<&str>,
        start: u64,
        end: u64,
    ) -> Result<OwnedRowsPage, MayoError> {
        self.snapshot()
            .await?
            .query_owned_page(metric_name, start, end)
            .await
    }

    /// See [`RollupSnapshot::query_cluster_metric`].
    pub async fn query_cluster_metric(
        &self,
        metric_name: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        self.snapshot()
            .await?
            .query_cluster_metric(metric_name, start, end)
            .await
    }

    /// See [`RollupSnapshot::query_all`].
    pub async fn query_all(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        self.snapshot().await?.query_all(start, end).await
    }

    /// See [`RollupSnapshot::query_cluster_aggregates`].
    pub async fn query_cluster_aggregates(
        &self,
        metric_name: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<ClusterAggregate>, MayoError> {
        self.snapshot()
            .await?
            .query_cluster_aggregates(metric_name, start, end)
            .await
    }

    /// See [`RollupSnapshot::metric_names`].
    pub async fn metric_names(&self) -> Result<Vec<String>, MayoError> {
        self.snapshot().await?.metric_names().await
    }

    /// Prune Parquet files whose newest datapoint is older than `before`.
    ///
    /// Keyed on the data's own newest timestamp (O12), read from row-group
    /// statistics, not the file's mtime — the rollup schema shares the
    /// leading `timestamp` column, so `store::file_max_timestamp` applies.
    ///
    /// Opening every file's footer is slow on a slow disk, so the scan runs
    /// on a blocking thread (#1417).
    pub async fn prune(&self, before: u64) -> Result<usize, MayoError> {
        let data_dir = self.data_dir.clone();
        let index = self.index.clone();
        tokio::task::spawn_blocking(move || {
            super::store::prune_directory(&data_dir, before, None, &index)
        })
        .await
        .map_err(|e| MayoError::Io(std::io::Error::other(e.to_string())))?
    }

    /// Prune rollups older than `retention`, measured back from `now`.
    ///
    /// A thin convenience over [`prune`](Self::prune) for the periodic
    /// maintenance tick: it turns the operator-facing `[metrics]
    /// rollup_retention_hours` (a `Duration`) into the absolute cutoff
    /// timestamp `prune` expects. A zero `retention` keeps everything —
    /// matching the "0 means unlimited" convention the other storage limits
    /// use — so an unconfigured council never deletes rollups by surprise.
    pub async fn prune_expired(
        &self,
        now: SystemTime,
        retention: Duration,
    ) -> Result<usize, MayoError> {
        if retention.is_zero() {
            return Ok(0);
        }
        let now_secs = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let before = now_secs.saturating_sub(retention.as_secs());
        self.prune(before).await
    }
}

/// What [`RollupStore::snapshot`] captured: the files and rollups a query
/// reads, independent of the store's lock.
pub struct RollupSnapshot {
    sources: Arc<Vec<ParquetSource>>,
    memory: Option<RecordBatch>,
    index: SourceIndex,
    #[cfg(test)]
    query_gate: Option<Arc<super::store::QueryGate>>,
}

impl RollupSnapshot {
    /// A session exposing the snapshot as the `rollups` table. Cheap: no
    /// I/O until a query runs.
    fn session(&self) -> Result<SessionContext, MayoError> {
        let ctx = streaming_session();
        let table = ParquetTable::new(
            Arc::new(rollup_schema()),
            Arc::clone(&self.sources),
            self.memory.clone(),
            None,
            "rollup",
            self.index.clone(),
        );
        ctx.register_table("rollups", Arc::new(table))
            .map_err(|e| MayoError::DataFusion(e.to_string()))?;
        Ok(ctx)
    }

    /// Query rollup data using SQL.
    ///
    /// Returns (timestamp, metric_name, labels, value) tuples where
    /// value is the sum across nodes for each (timestamp, metric_name, labels).
    pub async fn query_sql(&self, sql: &str) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        let ctx = self.session()?;
        let batches = super::store::collect_sql(&ctx, sql).await?;

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

    /// Worker contributions in `[start, end]`, every page of them, without
    /// losing the identity needed for reassignment deduplication. Refuses a
    /// range of more than [`MAX_OWNED_ROWS`] rather than return a truncated
    /// cluster sum.
    pub async fn query_owned_rows(
        &self,
        metric_name: Option<&str>,
        start: u64,
        end: u64,
    ) -> Result<Vec<OwnedRollupRow>, MayoError> {
        let mut rows = Vec::new();
        let mut cursor = start;
        loop {
            let page = self.query_owned_page(metric_name, cursor, end).await?;
            rows.extend(page.rows);
            if rows.len() > MAX_OWNED_ROWS {
                return Err(too_many_contributions());
            }
            match page.next_start {
                Some(next) if next > cursor => cursor = next,
                _ => return Ok(rows),
            }
        }
    }

    /// One page of worker contributions from `start`, oldest first: at most
    /// [`OWNED_ROWS_PAGE`] rows, ending on a whole minute, and the `start`
    /// of the next page if there is more (#2451). A single minute holding
    /// more than a page is refused: it can't be paged, and filtering by
    /// metric name narrows it.
    pub async fn query_owned_page(
        &self,
        metric_name: Option<&str>,
        start: u64,
        end: u64,
    ) -> Result<OwnedRowsPage, MayoError> {
        let name_filter = metric_name
            .map(|name| {
                format!(
                    " AND metric_name = '{}'",
                    super::store::escape_sql_literal(name)
                )
            })
            .unwrap_or_default();
        let end = end.min(i64::MAX as u64);
        let sql = format!(
            "SELECT DISTINCT timestamp, node_id, metric_name, labels, sum_val FROM rollups WHERE timestamp >= {start} AND timestamp <= {end}{name_filter} ORDER BY timestamp, node_id, metric_name, labels LIMIT {}",
            OWNED_ROWS_PAGE + 1
        );
        let batches = super::store::collect_sql(&self.session()?, &sql).await?;
        let mut rows = Vec::new();
        for batch in batches {
            let timestamps = batch
                .column(0)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .ok_or_else(|| MayoError::Arrow("timestamp column type mismatch".into()))?;
            let nodes = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| MayoError::Arrow("node_id column type mismatch".into()))?;
            let names = batch
                .column(2)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| MayoError::Arrow("metric_name column type mismatch".into()))?;
            let labels = batch
                .column(3)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| MayoError::Arrow("labels column type mismatch".into()))?;
            let values = batch
                .column(4)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| MayoError::Arrow("value column type mismatch".into()))?;
            for i in 0..batch.num_rows() {
                rows.push(OwnedRollupRow {
                    node_id: nodes.value(i).to_owned(),
                    row: MetricsQueryRow {
                        timestamp: timestamps.value(i),
                        metric_name: names.value(i).to_owned(),
                        labels: labels.value(i).to_owned(),
                        value: values.value(i),
                    },
                });
            }
        }
        if rows.len() <= OWNED_ROWS_PAGE {
            return Ok(OwnedRowsPage {
                rows,
                next_start: None,
            });
        }
        // The page ends before the first minute it couldn't hold whole.
        let next = rows[OWNED_ROWS_PAGE].row.timestamp;
        rows.retain(|owned| owned.row.timestamp < next);
        if rows.is_empty() {
            return Err(MayoError::QueryRejected(format!(
                "one minute holds more than {OWNED_ROWS_PAGE} rollup contributions; filter by metric name"
            )));
        }
        Ok(OwnedRowsPage {
            rows,
            next_start: Some(next),
        })
    }

    /// Query cluster-wide aggregated metrics by name and time range.
    ///
    /// Returns (timestamp, metric_name, labels, sum) tuples aggregated
    /// across all nodes, oldest first, at most [`ALL_QUERY_ROW_LIMIT`] of
    /// them, the newest. For averages, callers can also retrieve count
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
             ORDER BY timestamp DESC LIMIT {ALL_QUERY_ROW_LIMIT}"
        );
        // Newest first under the limit, then back to oldest first: without
        // a limit one request returned the whole rollup retention (#2382).
        let mut rows = self.query_sql(&sql).await?;
        rows.reverse();
        Ok(rows)
    }

    /// Every series' cluster sums in `[start, end]`, oldest first, at most
    /// 10,000 rows: what `/v1/metrics/rollup` returns without a name.
    pub async fn query_all(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        let sql = format!(
            "SELECT timestamp, metric_name, labels, SUM(sum_val) as total_sum \
             FROM rollups \
             WHERE timestamp >= {start} AND timestamp <= {end} \
             GROUP BY timestamp, metric_name, labels \
             ORDER BY timestamp LIMIT 10000"
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

        let ctx = self.session()?;
        #[cfg(test)]
        super::store::QueryGate::pass(&self.query_gate).await;
        let batches = super::store::collect_sql(&ctx, &sql).await?;

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
        let ctx = self.session()?;
        let batches = super::store::collect_sql(
            &ctx,
            "SELECT DISTINCT metric_name FROM rollups ORDER BY metric_name",
        )
        .await?;

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
}

/// The most worker contributions one page of `/v1/metrics/rollup/owned`
/// holds (#2451).
pub const OWNED_ROWS_PAGE: usize = 10_000;

/// The most worker contributions one cluster query reads from one member,
/// over all its pages (#2451).
pub const MAX_OWNED_ROWS: usize = 200_000;

/// The refusal for a range with more than [`MAX_OWNED_ROWS`] contributions.
pub(crate) fn too_many_contributions() -> MayoError {
    MayoError::QueryRejected(format!(
        "the range holds more than {MAX_OWNED_ROWS} rollup contributions; narrow the time range or filter by metric name"
    ))
}

/// One page of worker contributions, and where the next page starts.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnedRowsPage {
    pub rows: Vec<OwnedRollupRow>,
    pub next_start: Option<u64>,
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

    #[tokio::test]
    async fn prune_reports_failed_deletions_and_preserves_survivors() {
        let (store, dir) = test_store();
        let blocked = dir.path().join("blocked.parquet");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("survivor"), b"preserve").unwrap();
        let error = store
            .prune(u64::MAX)
            .await
            .expect_err("failed removal must not count as deletion");
        assert!(error.to_string().contains("blocked.parquet"), "{error}");
        assert_eq!(
            std::fs::read(blocked.join("survivor")).unwrap(),
            b"preserve"
        );
    }

    fn test_store() -> (RollupStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = RollupStore::new(dir.path().to_path_buf());
        (store, dir)
    }

    /// Four flushed files whose windows overlap and arrive out of order (as
    /// reassignment backfill delivers them), a corrupt file, and a buffer.
    async fn golden_rollups(store: &mut RollupStore, directory: &std::path::Path) {
        let files: [&[u64]; 5] = [&[0, 1, 2], &[5, 3], &[4, 1, 6], &[8, 7], &[9, 6]];
        // Older than the idempotency horizon, so a restart forgets it.
        store.ingest(&make_rollup("worker-a", 50_000, "cpu", 4.0));
        for (index, windows) in files.iter().enumerate() {
            for window in windows.iter() {
                for (node_index, node) in ["worker-a", "worker-b", "worker-c"].iter().enumerate() {
                    let timestamp = 60_000 + window * 60;
                    let mut entries = Vec::new();
                    for (metric_index, metric) in ["cpu", "mem", "disk"].iter().enumerate() {
                        if (window + metric_index as u64 + index as u64) % 4 == 3 {
                            continue;
                        }
                        let base =
                            (window * 7 + node_index as u64 * 3 + metric_index as u64) as f64;
                        entries.push(RollupEntry {
                            metric_name: metric.to_string(),
                            labels: BTreeMap::from([(
                                "app".to_string(),
                                format!("default/app-{}", (window + node_index as u64) % 2),
                            )]),
                            aggregate: RollupAggregate {
                                min: base,
                                max: base + 2.5,
                                sum: base * 6.0,
                                count: 6,
                            },
                        });
                    }
                    store.ingest(&NodeRollup {
                        node_id: NodeId::new(*node),
                        timestamp,
                        part: 0,
                        entries,
                    });
                }
            }
            if index + 1 < files.len() {
                store.flush().await.unwrap();
            }
        }
        std::fs::write(directory.join("rollup_999999.parquet"), b"not parquet").unwrap();
    }

    fn canonical_rows(rows: &[(u64, String, String, f64)]) -> String {
        assert!(rows.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        let mut lines: Vec<String> = rows.iter().map(|row| format!("{row:?}")).collect();
        lines.sort();
        lines.join("\n")
    }

    fn canonical_debug<T: std::fmt::Debug>(rows: &[T]) -> String {
        let mut lines: Vec<String> = rows.iter().map(|row| format!("{row:?}")).collect();
        lines.sort();
        lines.join("\n")
    }

    /// #377: streaming must return exactly what loading every file returned.
    /// The snapshot was recorded from the eager implementation before the
    /// change.
    #[tokio::test]
    async fn every_rollup_read_matches_the_eager_golden_answers() {
        let (mut store, directory) = test_store();
        golden_rollups(&mut store, directory.path()).await;
        let unbounded = i64::MAX as u64;
        let mut report = Vec::new();
        report.push(format!(
            "## cluster cpu unbounded\n{}",
            canonical_rows(
                &store
                    .query_cluster_metric("cpu", 0, unbounded)
                    .await
                    .unwrap()
            )
        ));
        report.push(format!(
            "## cluster mem window\n{}",
            canonical_rows(
                &store
                    .query_cluster_metric("mem", 60_120, 60_300)
                    .await
                    .unwrap()
            )
        ));
        report.push(format!(
            "## all unbounded\n{}",
            canonical_rows(&store.query_all(0, unbounded).await.unwrap())
        ));
        report.push(format!(
            "## all window\n{}",
            canonical_rows(&store.query_all(60_240, 60_420).await.unwrap())
        ));
        report.push(format!(
            "## aggregates disk\n{}",
            canonical_debug(
                &store
                    .query_cluster_aggregates("disk", 60_060, 60_480)
                    .await
                    .unwrap()
            )
        ));
        report.push(format!(
            "## owned cpu window\n{}",
            canonical_debug(
                &store
                    .query_owned_rows(Some("cpu"), 60_180, 60_360)
                    .await
                    .unwrap()
            )
        ));
        report.push(format!(
            "## owned unbounded\n{}",
            canonical_debug(&store.query_owned_rows(None, 0, unbounded).await.unwrap())
        ));
        report.push(format!(
            "## names\n{:?}",
            store.metric_names().await.unwrap()
        ));

        store.flush().await.unwrap();
        let mut restarted = RollupStore::new(directory.path().to_path_buf());
        restarted.hydrate_seen_windows().await.unwrap();
        let mut seen: Vec<_> = restarted.hydrated_windows.iter().cloned().collect();
        seen.sort();
        report.push(format!(
            "## hydrated windows (newest {})\n{seen:?}",
            restarted.newest_window
        ));
        insta::assert_snapshot!("rollup_golden_reads", report.join("\n\n"));
    }

    /// #2536: the store names the newest window each node sent, whatever the
    /// metric, a re-sent window included, and a restart reads it back from
    /// the flushed files.
    #[tokio::test]
    async fn reporters_name_each_nodes_newest_window_across_a_restart() {
        let (mut store, directory) = test_store();
        store.ingest(&make_rollup("w1", 120, "cpu", 1.0));
        store.ingest(&make_rollup("w1", 180, "mem", 1.0));
        store.ingest(&make_rollup("w2", 60, "cpu", 1.0));
        // A duplicate is dropped, but still tells us w2 is talking.
        assert!(!store.ingest(&make_rollup("w2", 60, "cpu", 1.0)));
        let sorted = |mut reporters: Vec<RollupReporter>| {
            reporters.sort_by(|a, b| a.node_id.cmp(&b.node_id));
            reporters
                .into_iter()
                .map(|reporter| (reporter.node_id, reporter.last_window))
                .collect::<Vec<_>>()
        };
        let expected = vec![("w1".to_string(), 180), ("w2".to_string(), 60)];
        assert_eq!(sorted(store.reporters()), expected);

        store.flush().await.unwrap();
        let mut restarted = RollupStore::new(directory.path().to_path_buf());
        restarted.hydrate_seen_windows().await.unwrap();
        assert_eq!(sorted(restarted.reporters()), expected);
    }

    fn make_rollup(node: &str, timestamp: u64, metric: &str, value: f64) -> NodeRollup {
        NodeRollup {
            node_id: NodeId::new(node),
            timestamp,
            part: 0,
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
            part: 0,
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
    async fn oversized_owned_queries_refuse_instead_of_truncating_the_sum() {
        // Paged since #2451: the refusal is at MAX_OWNED_ROWS, not one page.
        let (mut store, _dir) = test_store();
        let most = MAX_OWNED_ROWS as u64;
        for window in 0..=most {
            store.ingest(&make_rollup("worker-a", window, "cpu", 1.0));
        }
        let error = store.query_owned_rows(None, 0, most).await.unwrap_err();
        assert!(matches!(error, MayoError::QueryRejected(_)), "{error}");
        assert!(error.to_string().contains("narrow the time range"));
        assert_eq!(
            store
                .query_owned_rows(Some("cpu"), 0, most - 1)
                .await
                .unwrap()
                .len(),
            MAX_OWNED_ROWS
        );
    }

    #[tokio::test]
    async fn reassigned_history_stays_unique_after_restart_and_retries() {
        let mut sources = Vec::new();
        for worker in ["worker-b", "worker-c"] {
            let (mut store, dir) = test_store();
            let common = make_rollup("worker-a", 1000, "cpu", 10.0);
            store.ingest(&common);
            store.ingest(&make_rollup(worker, 1000, "cpu", 20.0));
            store.flush().await.unwrap();
            drop(store);
            let mut reopened = RollupStore::new(dir.path().to_path_buf());
            // Even a retry before hydrating in-memory receipt state must not double-count.
            reopened.ingest(&common);
            reopened.flush().await.unwrap();
            assert!(
                reopened
                    .query_owned_rows(Some("cpu' OR '1'='1"), 0, 2000)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let rows = reopened
                .query_owned_rows(Some("cpu"), 0, 2000)
                .await
                .unwrap();
            assert_eq!(rows.len(), 2);
            sources.push(rows);
        }
        let result = crate::mayo::query_fanout::merge_owned_rollups(sources);
        assert!(result.warnings.is_empty());
        assert_eq!(result.data.len(), 1);
        assert_eq!(result.data[0].value, 50.0);
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

    /// M17: after a restart, a re-sent (node, window) that was already flushed
    /// to Parquet must not be counted a second time. A fresh store over the
    /// same directory, once its idempotency tracker is seeded from disk, rejects
    /// the duplicate so the cluster sum stays correct.
    #[tokio::test]
    async fn restart_does_not_double_count_a_reflushed_window() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = RollupStore::new(dir.path().to_path_buf());
            store.ingest(&make_rollup("n1", 1000, "cpu", 42.0));
            store.flush().await.unwrap();
        }

        // Simulate a restart: a new store over the same dir. Without seeding it
        // would happily re-ingest the same window.
        let mut restarted = RollupStore::new(dir.path().to_path_buf());
        restarted.hydrate_seen_windows().await.unwrap();
        let accepted = restarted.ingest(&make_rollup("n1", 1000, "cpu", 42.0));
        assert!(!accepted, "a window already on disk must be a duplicate");
        restarted.flush().await.unwrap();

        // The sum reflects the single ingest, not double.
        let results = restarted
            .query_cluster_metric("cpu", 0, 9999)
            .await
            .unwrap();
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

    /// #2382: `/v1/metrics/rollup?name=X` over all time had no row bound.
    /// It keeps the newest `ALL_QUERY_ROW_LIMIT` sums, oldest first.
    #[tokio::test]
    async fn a_named_rollup_query_returns_at_most_the_row_limit() {
        let (mut store, _dir) = test_store();
        let total = crate::mayo::store::ALL_QUERY_ROW_LIMIT as u64 + 50;
        for minute in 1..=total {
            store.ingest(&make_rollup("n1", minute * 60, "cpu", 1.0));
        }
        let rows = store
            .query_cluster_metric("cpu", 0, i64::MAX as u64)
            .await
            .unwrap();
        assert_eq!(rows.len(), crate::mayo::store::ALL_QUERY_ROW_LIMIT);
        assert_eq!(rows.last().unwrap().0, total * 60, "newest kept");
        assert!(rows.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }

    /// #2451: a member refused any cluster query over 10,000 contributions,
    /// and one series on ten nodes is 14,400 a day. The query now reads in
    /// pages and succeeds.
    #[tokio::test]
    async fn a_cluster_query_over_a_day_of_one_series_on_ten_nodes_succeeds() {
        let (mut store, _dir) = test_store();
        for node in 0..10 {
            for minute in 1..=1_440 {
                store.ingest(&make_rollup(
                    &format!("node-{node}"),
                    minute * 60,
                    "node_cpu_usage_percent",
                    1.0,
                ));
            }
        }
        let rows = store
            .query_owned_rows(Some("node_cpu_usage_percent"), 0, i64::MAX as u64)
            .await
            .unwrap();
        let merged = crate::mayo::query_fanout::merge_owned_rollups(vec![rows]);
        assert!(merged.warnings.is_empty(), "{:?}", merged.warnings);
        assert_eq!(merged.data.len(), 1_440);
        assert!(merged.data.iter().all(|row| row.value == 10.0));
    }

    /// #2451: a page holds whole minutes only, so the next page starts
    /// where it stopped and no minute is split between two.
    #[tokio::test]
    async fn an_owned_rows_page_ends_on_a_whole_minute() {
        let (mut store, _dir) = test_store();
        for node in 0..3 {
            for minute in 1..=5_000 {
                store.ingest(&make_rollup(&format!("n{node}"), minute * 60, "cpu", 1.0));
            }
        }
        let page = store
            .snapshot()
            .await
            .unwrap()
            .query_owned_page(Some("cpu"), 0, i64::MAX as u64)
            .await
            .unwrap();
        let next = page.next_start.expect("15,000 rows take two pages");
        assert!(page.rows.len() <= OWNED_ROWS_PAGE);
        assert_eq!(page.rows.len() % 3, 0, "a minute was split");
        assert!(page.rows.iter().all(|row| row.row.timestamp < next));
    }

    /// #2451: one minute with more contributions than a page can't be
    /// paged; it's refused as the caller's to narrow, not as a failure.
    #[tokio::test]
    async fn a_minute_larger_than_a_page_is_refused() {
        let (mut store, _dir) = test_store();
        for node in 0..=OWNED_ROWS_PAGE {
            store.ingest(&make_rollup(&format!("n{node}"), 60, "cpu", 1.0));
        }
        let error = store
            .query_owned_rows(Some("cpu"), 0, 1_000)
            .await
            .unwrap_err();
        assert!(matches!(error, MayoError::QueryRejected(_)), "{error}");
    }

    /// #2426: a minute too large for one frame arrives in parts. Each part
    /// is ingested once; a re-sent part is a duplicate.
    #[tokio::test]
    async fn each_part_of_a_minute_is_ingested_once() {
        let (mut store, _dir) = test_store();
        let part = |part: u16, metric: &str| NodeRollup {
            part,
            ..make_rollup("n1", 60, metric, 1.0)
        };
        assert!(store.ingest(&part(0, "cpu")));
        assert!(store.ingest(&part(1, "mem")));
        assert!(!store.ingest(&part(1, "mem")), "a re-sent part");
        assert_eq!(store.buffer_len(), 2);
    }

    /// #2443: a flush written outside the store lock keeps its rows
    /// queryable while it runs, and once it completes they are read from
    /// the file, never twice.
    #[tokio::test]
    async fn a_rollup_flush_outside_the_lock_reads_each_row_once() {
        let (mut store, _dir) = test_store();
        store.ingest(&make_rollup("n1", 60, "cpu", 1.0));
        let pending = store.take_flush_batch().unwrap().unwrap();
        store.ingest(&make_rollup("n1", 120, "cpu", 2.0));
        assert_eq!(store.buffer_len(), 2);
        let sums = |rows: Vec<(u64, String, String, f64)>| -> Vec<(u64, f64)> {
            rows.into_iter().map(|row| (row.0, row.3)).collect()
        };
        let before = store.query_cluster_metric("cpu", 0, 1_000).await.unwrap();
        assert_eq!(sums(before), [(60, 1.0), (120, 2.0)]);

        write_rollup_pending(&pending).await.unwrap();
        // Written but not yet completed: still read once, from memory.
        let during = store.query_cluster_metric("cpu", 0, 1_000).await.unwrap();
        assert_eq!(sums(during), [(60, 1.0), (120, 2.0)]);
        store.complete_flush(&pending);
        let after = store.query_cluster_metric("cpu", 0, 1_000).await.unwrap();
        assert_eq!(sums(after), [(60, 1.0), (120, 2.0)]);
        assert_eq!(store.buffer_len(), 1);
    }

    /// #2443: the flush task no longer holds the write lock through the
    /// Parquet write, so ingest goes on.
    #[tokio::test]
    async fn flushing_off_the_lock_persists_and_empties_the_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let store = tokio::sync::RwLock::new(RollupStore::new(dir.path().to_path_buf()));
        store
            .write()
            .await
            .ingest(&make_rollup("n1", 60, "cpu", 1.0));
        flush_off_lock(&store).await.unwrap();
        assert_eq!(store.read().await.buffer_len(), 0);
        let reopened = RollupStore::new(dir.path().to_path_buf());
        let rows = reopened
            .query_cluster_metric("cpu", 0, 1_000)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    /// #2450: shedding from a full buffer moves no buffered row, and keeps
    /// the newest windows.
    #[tokio::test]
    async fn shedding_rollups_moves_nothing_and_keeps_the_newest() {
        let (mut store, _dir) = test_store();
        store.max_buffer_rows = 100;
        for minute in 1..=250 {
            store.ingest(&make_rollup("n1", minute * 60, "cpu", 1.0));
        }
        assert_eq!(store.buffer_len(), 100);
        let rows = store.query_cluster_metric("cpu", 0, 100_000).await.unwrap();
        assert_eq!(rows.first().unwrap().0, 151 * 60);
        assert_eq!(rows.last().unwrap().0, 250 * 60);
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

    #[tokio::test]
    async fn prune_removes_old_rollup_files() {
        let (mut store, _dir) = test_store();
        store.ingest(&make_rollup("n1", 1000, "cpu", 10.0));
        store.flush().await.unwrap();

        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 10_000;
        assert_eq!(store.prune(future).await.unwrap(), 1);

        // Pruning to the epoch keeps everything.
        store.ingest(&make_rollup("n2", 1000, "cpu", 20.0));
        store.flush().await.unwrap();
        assert_eq!(store.prune(0).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn prune_expired_drops_rollups_past_retention() {
        // A rollup written with an old window timestamp is dropped once its age
        // exceeds the retention, while a fresh one survives.
        let (mut store, _dir) = test_store();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // One window two hours old, flushed to its own file.
        store.ingest(&make_rollup("n1", now - 7200, "cpu", 10.0));
        store.flush().await.unwrap();

        // Retaining one hour must delete the two-hour-old file.
        let deleted = store
            .prune_expired(std::time::SystemTime::now(), Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(deleted, 1);

        // A zero retention keeps everything, even stale data.
        store.ingest(&make_rollup("n2", now - 7200, "cpu", 20.0));
        store.flush().await.unwrap();
        assert_eq!(
            store
                .prune_expired(std::time::SystemTime::now(), Duration::ZERO)
                .await
                .unwrap(),
            0
        );
    }

    /// The disk-pressure tick prunes expired rollups, and pruning opens
    /// every Parquet file's footer. It ran on a runtime worker, under the
    /// store's read guard, on the disks where 0.2.6's soaks saw journal
    /// writes take over a second (#1606). With the blocking pool held busy,
    /// a pending first poll means the files are read on a blocking thread;
    /// work done inline would finish inside that poll (#1674).
    #[test]
    fn expiry_reads_the_files_off_the_runtime() {
        let (store, dir) = test_store();
        for file in 0..500 {
            std::fs::write(
                dir.path().join(format!("rollup_{file:06}.parquet")),
                [0u8; 10],
            )
            .unwrap();
        }
        let pruned = crate::off_runtime::assert_runs_on_blocking_pool(
            store.prune_expired(std::time::SystemTime::now(), Duration::from_secs(3600)),
        );
        // Unreadable footers fall back to the files' mtimes, which are new.
        assert_eq!(pruned.unwrap(), 0);
    }

    #[test]
    fn cluster_aggregate_avg_zero_count_is_none() {
        let agg = ClusterAggregate {
            timestamp: 1,
            metric_name: "cpu".to_string(),
            labels: "{}".to_string(),
            min: 0.0,
            max: 0.0,
            sum: 0.0,
            count: 0,
        };
        assert_eq!(agg.avg(), None);
    }

    #[tokio::test]
    async fn cluster_aggregates_query_neutralises_injection() {
        // OBS1: the aggregates path escapes the metric name too.
        let (mut store, _dir) = test_store();
        store.ingest(&make_rollup("n1", 1000, "cpu", 10.0));
        store.flush().await.unwrap();

        let injected = store
            .query_cluster_aggregates("x' OR '1'='1", 0, 9999)
            .await
            .unwrap();
        assert!(injected.is_empty(), "injection matched rows: {injected:?}");
    }

    #[tokio::test]
    async fn idempotency_tracker_stays_bounded() {
        // OBS2: the (node, window) tracker is pruned to a horizon so it doesn't
        // grow without bound over a long-running process.
        let (mut store, _dir) = test_store();
        let horizon = IDEMPOTENCY_HORIZON_SECS;
        // Ingest windows spread far apart so old ones fall past the horizon.
        for i in 0..(MAX_SEEN_WINDOWS as u64 + 10) {
            store.ingest(&make_rollup("n1", i * horizon, "cpu", 1.0));
        }
        assert!(
            store.seen_windows.len() <= MAX_SEEN_WINDOWS,
            "tracker grew past the cap: {}",
            store.seen_windows.len()
        );
    }
}
