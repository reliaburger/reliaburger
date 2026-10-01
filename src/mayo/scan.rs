//! Streamed reads over Mayo's Parquet files (#377).
//!
//! A [`ParquetTable`] is a DataFusion table whose rows come straight from
//! the flushed Parquet files (local paths or object-store keys) plus the
//! unflushed buffer, one file at a time. Nothing is loaded up front: when a
//! query runs, each file's footer is checked against the query's time and
//! metric-name predicates, files and row groups that can't match are
//! skipped without reading their data, and the rest are decoded with only
//! the columns the query uses. DataFusion's filter, aggregate and top-k
//! operators consume the stream as it arrives, so a query's peak memory is
//! its working set (one file plus the operator's state), not the history on
//! disk.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::{RecordBatch, RecordBatchOptions};
use datafusion::catalog::{Session, TableProvider};
use datafusion::common::{DataFusionError, ScalarValue};
use datafusion::execution::TaskContext;
use datafusion::logical_expr::expr::{Between, InList};
use datafusion::logical_expr::{
    BinaryExpr, Expr, Operator, TableProviderFilterPushDown, TableType,
};
use datafusion::parquet::arrow::ProjectionMask;
use datafusion::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use datafusion::parquet::file::metadata::{ParquetMetaData, ParquetMetaDataReader};
use datafusion::parquet::file::reader::ChunkReader;
use datafusion::parquet::file::statistics::Statistics;
use datafusion::parquet::schema::types::SchemaDescriptor;
use datafusion::physical_expr::LexOrdering;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::streaming::{PartitionStream, StreamingTableExec};
use datafusion::physical_plan::{ExecutionPlan, SendableRecordBatchStream};
use datafusion::prelude::{SessionConfig, SessionContext};
use futures_util::StreamExt;
use object_store::{ObjectStore, ObjectStoreExt};

use super::types::MayoError;

/// The column every Mayo table is pruned by time on.
const TIMESTAMP_COLUMN: &str = "timestamp";
/// The column every Mayo table is pruned by metric name on.
const METRIC_NAME_COLUMN: &str = "metric_name";

/// A DataFusion session for streamed Mayo queries.
///
/// One partition: DataFusion would otherwise split the scan across a
/// partition per CPU behind a repartition step, whose reader runs ahead of
/// the slower consumers and buffers batches, so the working set grew with
/// the files read rather than staying at one file. Mayo's queries are small
/// and per node; a single thread reads them comfortably.
pub(crate) fn streaming_session() -> SessionContext {
    SessionContext::new_with_config(SessionConfig::new().with_target_partitions(1))
}

/// One flushed Parquet file a [`ParquetTable`] reads.
#[derive(Debug, Clone)]
pub(crate) enum ParquetSource {
    /// A file in the store's local directory.
    Local(PathBuf),
    /// An object in a bucket, with its size from the listing so the footer
    /// can be fetched on its own.
    Remote {
        store: Arc<dyn ObjectStore>,
        location: object_store::path::Path,
        size: u64,
    },
}

impl ParquetSource {
    fn describe(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
            Self::Remote { location, .. } => location.to_string(),
        }
    }
}

/// Every `*.parquet` file in `directory`, in directory order. A directory
/// that doesn't exist yet (no flush so far) holds none.
pub(crate) fn list_local(directory: &std::path::Path) -> Result<Vec<ParquetSource>, MayoError> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(MayoError::Io(error)),
    };
    Ok(entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|x| x == "parquet"))
        .map(ParquetSource::Local)
        .collect())
}

/// Every `{prefix}_*.parquet` object under `location` in a bucket, in
/// listing order. Only names and sizes are held, never data.
pub(crate) async fn list_remote(
    store: &Arc<dyn ObjectStore>,
    location: &object_store::path::Path,
    prefix: &str,
) -> Result<Vec<ParquetSource>, MayoError> {
    let mut sources = Vec::new();
    let mut listing = store.list(Some(location));
    while let Some(item) = listing.next().await {
        let meta = item.map_err(|e| MayoError::ObjectStore(e.to_string()))?;
        let name = meta.location.filename().unwrap_or("");
        if name.starts_with(&format!("{prefix}_")) && name.ends_with(".parquet") {
            sources.push(ParquetSource::Remote {
                store: Arc::clone(store),
                location: meta.location,
                size: meta.size,
            });
        }
    }
    Ok(sources)
}

/// What a query's predicates promise about the rows it wants, in the terms
/// Parquet statistics can answer: an inclusive time range and a set of
/// metric names. `None` means unconstrained. Anything the predicates say
/// that doesn't fit these terms is left to DataFusion's own filter, so a
/// bound here only ever skips data no row of the answer could come from.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ScanBounds {
    /// Lowest timestamp the query can want.
    pub since: Option<u64>,
    /// Highest timestamp the query can want.
    pub until: Option<u64>,
    /// The only metric names the query can want.
    pub names: Option<BTreeSet<String>>,
}

impl ScanBounds {
    /// Bounds implied by the conjunction of `filters`, as DataFusion pushes
    /// them to a table scan.
    pub fn from_filters(filters: &[Expr]) -> Self {
        let mut bounds = Self::default();
        for filter in filters {
            bounds.narrow(filter);
        }
        bounds
    }

    /// Raise the lowest timestamp to at least `since`.
    pub fn raise_since(&mut self, since: u64) {
        self.since = Some(self.since.map_or(since, |current| current.max(since)));
    }

    fn lower_until(&mut self, until: u64) {
        self.until = Some(self.until.map_or(until, |current| current.min(until)));
    }

    fn restrict_names(&mut self, names: BTreeSet<String>) {
        self.names = Some(match self.names.take() {
            Some(current) => current.intersection(&names).cloned().collect(),
            None => names,
        });
    }

    /// The smallest bounds that hold every row either `self` or `other`
    /// admits.
    fn hull(self, other: Self) -> Self {
        Self {
            since: self.since.zip(other.since).map(|(a, b)| a.min(b)),
            until: self.until.zip(other.until).map(|(a, b)| a.max(b)),
            names: self
                .names
                .zip(other.names)
                .map(|(a, b)| a.union(&b).cloned().collect()),
        }
    }

    /// Narrow `self` by every bound `other` sets.
    fn intersect(&mut self, other: Self) {
        if let Some(since) = other.since {
            self.raise_since(since);
        }
        if let Some(until) = other.until {
            self.lower_until(until);
        }
        if let Some(names) = other.names {
            self.restrict_names(names);
        }
    }

    fn narrow(&mut self, filter: &Expr) {
        match filter {
            Expr::BinaryExpr(BinaryExpr {
                left,
                op: Operator::And,
                right,
            }) => {
                self.narrow(left);
                self.narrow(right);
            }
            // A row matching either side lies within the hull of the two
            // sides' bounds. DataFusion rewrites short IN lists this way.
            Expr::BinaryExpr(BinaryExpr {
                left,
                op: Operator::Or,
                right,
            }) => {
                let left = Self::from_filters(std::slice::from_ref(left));
                let right = Self::from_filters(std::slice::from_ref(right));
                self.intersect(left.hull(right));
            }
            Expr::BinaryExpr(BinaryExpr { left, op, right }) => {
                self.narrow_comparison(left, *op, right);
            }
            Expr::Between(Between {
                expr,
                negated: false,
                low,
                high,
            }) if column_name(expr) == Some(TIMESTAMP_COLUMN) => {
                if let Some(low) = literal_u64(low) {
                    self.raise_since(low);
                }
                if let Some(high) = literal_u64(high) {
                    self.lower_until(high);
                }
            }
            Expr::InList(InList {
                expr,
                list,
                negated: false,
            }) if column_name(expr) == Some(METRIC_NAME_COLUMN) => {
                let names: Option<BTreeSet<String>> = list.iter().map(literal_string).collect();
                if let Some(names) = names {
                    self.restrict_names(names);
                }
            }
            _ => {}
        }
    }

    /// `column op literal`, in either order.
    fn narrow_comparison(&mut self, left: &Expr, op: Operator, right: &Expr) {
        let (column, op, value) = match (column_name(left), column_name(right)) {
            (Some(column), None) => (column, op, right),
            (None, Some(column)) => match op.swap() {
                Some(swapped) => (column, swapped, left),
                None => return,
            },
            _ => return,
        };
        if column == METRIC_NAME_COLUMN {
            if let (Operator::Eq, Some(name)) = (op, literal_string(value)) {
                self.restrict_names(BTreeSet::from([name]));
            }
            return;
        }
        if column != TIMESTAMP_COLUMN {
            return;
        }
        let Some(value) = literal_u64(value) else {
            return;
        };
        match op {
            Operator::Eq => {
                self.raise_since(value);
                self.lower_until(value);
            }
            Operator::GtEq => self.raise_since(value),
            Operator::Gt => {
                if let Some(next) = value.checked_add(1) {
                    self.raise_since(next);
                }
            }
            Operator::LtEq => self.lower_until(value),
            Operator::Lt => {
                if let Some(previous) = value.checked_sub(1) {
                    self.lower_until(previous);
                }
            }
            _ => {}
        }
    }

    /// Whether rows whose timestamps span `[min, max]` can match. Missing
    /// statistics admit everything.
    fn admits_times(&self, min: Option<u64>, max: Option<u64>) -> bool {
        if let (Some(since), Some(until)) = (self.since, self.until)
            && since > until
        {
            return false;
        }
        if let (Some(since), Some(max)) = (self.since, max)
            && max < since
        {
            return false;
        }
        if let (Some(until), Some(min)) = (self.until, min)
            && min > until
        {
            return false;
        }
        true
    }

    /// Whether rows whose metric names span `[min, max]` (Parquet orders
    /// UTF-8 statistics byte by byte) can match.
    fn admits_names(&self, min: Option<&[u8]>, max: Option<&[u8]>) -> bool {
        let Some(names) = &self.names else {
            return true;
        };
        names.iter().any(|name| {
            let name = name.as_bytes();
            min.is_none_or(|min| name >= min) && max.is_none_or(|max| name <= max)
        })
    }
}

/// The column a predicate side names, if it's a bare column.
fn column_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Column(column) => Some(column.name.as_str()),
        _ => None,
    }
}

/// A non-negative integer literal as a timestamp. Anything else (a
/// negative, a float, a cast we don't follow) gives no bound, which is
/// always safe.
fn literal_u64(expr: &Expr) -> Option<u64> {
    let Expr::Literal(value, _) = expr else {
        return None;
    };
    match value {
        ScalarValue::UInt64(Some(v)) => Some(*v),
        ScalarValue::UInt32(Some(v)) => Some(u64::from(*v)),
        ScalarValue::UInt16(Some(v)) => Some(u64::from(*v)),
        ScalarValue::UInt8(Some(v)) => Some(u64::from(*v)),
        ScalarValue::Int64(Some(v)) => u64::try_from(*v).ok(),
        ScalarValue::Int32(Some(v)) => u64::try_from(*v).ok(),
        ScalarValue::Int16(Some(v)) => u64::try_from(*v).ok(),
        ScalarValue::Int8(Some(v)) => u64::try_from(*v).ok(),
        _ => None,
    }
}

fn literal_string(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Literal(
            ScalarValue::Utf8(Some(value))
            | ScalarValue::LargeUtf8(Some(value))
            | ScalarValue::Utf8View(Some(value)),
            _,
        ) => Some(value.clone()),
        _ => None,
    }
}

/// The leaf index of a top-level column in a file's Parquet schema.
fn leaf_index(schema: &SchemaDescriptor, name: &str) -> Option<usize> {
    schema
        .columns()
        .iter()
        .position(|column| column.name() == name)
}

/// The row groups of a file that can hold rows within `bounds`, judged from
/// their statistics alone.
fn admitted_row_groups(metadata: &ParquetMetaData, bounds: &ScanBounds) -> Vec<usize> {
    let schema = metadata.file_metadata().schema_descr();
    let timestamp = leaf_index(schema, TIMESTAMP_COLUMN);
    let name = leaf_index(schema, METRIC_NAME_COLUMN);
    (0..metadata.num_row_groups())
        .filter(|index| {
            let group = metadata.row_group(*index);
            let (min_time, max_time) = match timestamp.and_then(|i| group.column(i).statistics()) {
                // Unsigned timestamps are stored as INT64; reinterpreting the
                // bits restores the value, and its ordering, exactly.
                Some(Statistics::Int64(stats)) => (
                    stats.min_opt().map(|v| *v as u64),
                    stats.max_opt().map(|v| *v as u64),
                ),
                _ => (None, None),
            };
            let (min_name, max_name) = match name.and_then(|i| group.column(i).statistics()) {
                // A truncated minimum is a prefix of the real one and a
                // truncated maximum is rounded up, so both still bound it.
                Some(Statistics::ByteArray(stats)) => (
                    stats.min_opt().map(|v| v.data()),
                    stats.max_opt().map(|v| v.data()),
                ),
                _ => (None, None),
            };
            bounds.admits_times(min_time, max_time) && bounds.admits_names(min_name, max_name)
        })
        .collect()
}

/// What one scan wants from each file: the columns (in the order and with
/// the types of the table's projected schema) and the bounds that prune.
#[derive(Debug)]
struct ScanPlan {
    schema: SchemaRef,
    bounds: ScanBounds,
    /// "metrics" or "rollup", for the log line when a file is skipped.
    kind: &'static str,
}

/// Decode the admitted row groups of one file, only the planned columns.
/// All of a file or none of it: a file that fails part-way is skipped
/// whole, as the eager reader did (OBS5).
fn decode<R: ChunkReader + 'static>(
    reader: R,
    plan: &ScanPlan,
) -> Result<Vec<RecordBatch>, String> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(reader).map_err(|e| e.to_string())?;
    let groups = admitted_row_groups(builder.metadata(), &plan.bounds);
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    let file_schema = builder.parquet_schema();
    // A query that needs no column (`COUNT(*)`) still needs row counts, so
    // it reads the narrowest one.
    let mut wanted: Vec<&str> = plan
        .schema
        .fields()
        .iter()
        .map(|field| field.name().as_str())
        .collect();
    if wanted.is_empty() {
        wanted.push(TIMESTAMP_COLUMN);
    }
    let leaves = wanted
        .iter()
        .map(|name| leaf_index(file_schema, name).ok_or(format!("no {name} column")))
        .collect::<Result<Vec<_>, _>>()?;
    let mask = ProjectionMask::leaves(file_schema, leaves);
    let reader = builder
        .with_row_groups(groups)
        .with_projection(mask)
        .build()
        .map_err(|e| e.to_string())?;

    let mut batches = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| e.to_string())?;
        // Reorder by name into the table's schema, which also restores the
        // canonical nullability the file's inferred schema may lack.
        let columns = plan
            .schema
            .fields()
            .iter()
            .map(|field| {
                batch
                    .column_by_name(field.name())
                    .cloned()
                    .ok_or(format!("no {} column", field.name()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let options = RecordBatchOptions::new().with_row_count(Some(batch.num_rows()));
        let batch = RecordBatch::try_new_with_options(Arc::clone(&plan.schema), columns, &options)
            .map_err(|e| e.to_string())?;
        batches.push(batch);
    }
    Ok(batches)
}

/// A bucket object's footer, fetched with two range reads instead of the
/// whole object. `None` when it can't be read, so the caller fetches the
/// object and lets the decoder decide.
async fn remote_footer(
    store: &Arc<dyn ObjectStore>,
    location: &object_store::path::Path,
    size: u64,
) -> Option<ParquetMetaData> {
    const FOOTER: u64 = 8;
    if size < FOOTER {
        return None;
    }
    let tail = store.get_range(location, size - FOOTER..size).await.ok()?;
    if tail.len() != FOOTER as usize || &tail[4..] != b"PAR1" {
        return None;
    }
    let length = u64::from(u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]));
    let metadata_start = size.checked_sub(FOOTER + length)?;
    let metadata = store
        .get_range(location, metadata_start..size - FOOTER)
        .await
        .ok()?;
    ParquetMetaDataReader::decode_metadata(&metadata).ok()
}

fn blocking_failed(error: tokio::task::JoinError) -> DataFusionError {
    DataFusionError::External(Box::new(std::io::Error::other(error.to_string())))
}

/// Read one source's admitted rows. A file that can't be read or decoded
/// is logged and contributes nothing; only a failed object download fails
/// the query, as before.
async fn read_source(
    source: ParquetSource,
    plan: Arc<ScanPlan>,
) -> Result<Vec<RecordBatch>, DataFusionError> {
    let described = source.describe();
    let decoded = match source {
        ParquetSource::Local(path) => {
            let plan = Arc::clone(&plan);
            tokio::task::spawn_blocking(move || {
                let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
                decode(file, &plan)
            })
            .await
            .map_err(blocking_failed)?
        }
        ParquetSource::Remote {
            store,
            location,
            size,
        } => {
            if let Some(footer) = remote_footer(&store, &location, size).await
                && admitted_row_groups(&footer, &plan.bounds).is_empty()
            {
                return Ok(Vec::new());
            }
            let bytes = match store.get(&location).await {
                Ok(result) => result
                    .bytes()
                    .await
                    .map_err(|e| DataFusionError::External(Box::new(e)))?,
                // Deleted between the listing and now.
                Err(_) => return Ok(Vec::new()),
            };
            let plan = Arc::clone(&plan);
            tokio::task::spawn_blocking(move || decode(bytes, &plan))
                .await
                .map_err(blocking_failed)?
        }
    };
    match decoded {
        Ok(batches) => Ok(batches),
        Err(error) => {
            eprintln!(
                "mayo: skipping unreadable {} file {described}: {error}",
                plan.kind
            );
            Ok(Vec::new())
        }
    }
}

/// The one partition of a [`ParquetTable`] scan: every source in order,
/// then the buffer.
#[derive(Debug)]
struct SourceStream {
    plan: Arc<ScanPlan>,
    sources: Arc<Vec<ParquetSource>>,
    buffer: Option<RecordBatch>,
}

impl PartitionStream for SourceStream {
    fn schema(&self) -> &SchemaRef {
        &self.plan.schema
    }

    fn execute(&self, _context: Arc<TaskContext>) -> SendableRecordBatchStream {
        let plan = Arc::clone(&self.plan);
        let sources = Arc::clone(&self.sources);
        // `then` runs one read at a time, so at most one file's batches are
        // decoded and waiting at once.
        let files = futures_util::stream::iter(0..sources.len())
            .then(move |index| read_source(sources[index].clone(), Arc::clone(&plan)))
            .flat_map(|read| {
                let batches: Vec<Result<RecordBatch, DataFusionError>> = match read {
                    Ok(batches) => batches.into_iter().map(Ok).collect(),
                    Err(error) => vec![Err(error)],
                };
                futures_util::stream::iter(batches)
            });
        let buffer = futures_util::stream::iter(self.buffer.clone().map(Ok));
        Box::pin(RecordBatchStreamAdapter::new(
            Arc::clone(&self.plan.schema),
            files.chain(buffer),
        ))
    }
}

/// A Mayo table (metrics or rollups) over Parquet sources and a buffer,
/// read lazily per query.
#[derive(Debug)]
pub(crate) struct ParquetTable {
    schema: SchemaRef,
    sources: Arc<Vec<ParquetSource>>,
    buffer: Option<RecordBatch>,
    /// A lower time bound every query on this table promises to apply.
    floor: Option<u64>,
    kind: &'static str,
}

impl ParquetTable {
    /// A table over `sources` then `buffer`. `floor` is a lower time bound
    /// the caller's SQL already filters on; `kind` names the files in logs.
    pub fn new(
        schema: SchemaRef,
        sources: Vec<ParquetSource>,
        buffer: Option<RecordBatch>,
        floor: Option<u64>,
        kind: &'static str,
    ) -> Self {
        Self {
            schema,
            sources: Arc::new(sources),
            buffer,
            floor,
            kind,
        }
    }
}

// `TableProvider` is declared with `#[async_trait]`, which turns its `async
// fn scan` into a method returning a boxed future, so the impl needs the
// same attribute to match.
#[async_trait]
impl TableProvider for ParquetTable {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    /// Every filter is offered to the scan, which prunes with what it
    /// understands; `Inexact` keeps DataFusion's own filter on top, so
    /// pruning only has to be safe, never complete.
    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> datafusion::common::Result<Vec<TableProviderFilterPushDown>> {
        Ok(vec![TableProviderFilterPushDown::Inexact; filters.len()])
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> datafusion::common::Result<Arc<dyn ExecutionPlan>> {
        let mut bounds = ScanBounds::from_filters(filters);
        if let Some(floor) = self.floor {
            bounds.raise_since(floor);
        }
        let indices: Vec<usize> = match projection {
            Some(indices) => indices.clone(),
            None => (0..self.schema.fields().len()).collect(),
        };
        let schema = Arc::new(self.schema.project(&indices)?);
        let buffer = match &self.buffer {
            Some(buffer) => Some(buffer.project(&indices)?),
            None => None,
        };
        let stream = SourceStream {
            plan: Arc::new(ScanPlan {
                schema: Arc::clone(&schema),
                bounds,
                kind: self.kind,
            }),
            sources: Arc::clone(&self.sources),
            buffer,
        };
        let exec = StreamingTableExec::try_new(
            schema,
            vec![Arc::new(stream)],
            None,
            None::<LexOrdering>,
            false,
            limit,
        )?;
        Ok(Arc::new(exec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion::common::tree_node::TreeNode;
    use datafusion::datasource::MemTable;
    use datafusion::logical_expr::LogicalPlan;
    use datafusion::prelude::SessionContext;

    /// The bounds DataFusion hands the scan for `sql`, read from the
    /// optimised plan's table scan: the exact filters `scan` receives.
    async fn bounds_for(sql: &str) -> ScanBounds {
        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::UInt64, false),
            Field::new("metric_name", DataType::Utf8, false),
            Field::new("labels", DataType::Utf8, false),
            Field::new("value", DataType::Float64, false),
        ]));
        let ctx = SessionContext::new();
        let table = ParquetTable::new(schema, Vec::new(), None, None, "metrics");
        ctx.register_table("metrics", Arc::new(table)).unwrap();
        let plan = ctx.sql(sql).await.unwrap().into_optimized_plan().unwrap();
        let mut found = None;
        plan.apply(|node| {
            if let LogicalPlan::TableScan(scan) = node {
                found = Some(ScanBounds::from_filters(&scan.filters));
            }
            Ok(datafusion::common::tree_node::TreeNodeRecursion::Continue)
        })
        .unwrap();
        found.expect("the plan has a table scan")
    }

    fn names(list: &[&str]) -> Option<BTreeSet<String>> {
        Some(list.iter().map(|name| name.to_string()).collect())
    }

    #[tokio::test]
    async fn time_range_and_name_reach_the_scan() {
        let bounds = bounds_for(
            "SELECT * FROM metrics WHERE metric_name = 'cpu' \
             AND timestamp >= 100 AND timestamp <= 200",
        )
        .await;
        assert_eq!(
            bounds,
            ScanBounds {
                since: Some(100),
                until: Some(200),
                names: names(&["cpu"]),
            }
        );
    }

    #[tokio::test]
    async fn strict_bounds_between_and_in_lists_narrow_too() {
        let bounds = bounds_for(
            "SELECT * FROM metrics WHERE metric_name IN ('cpu', 'mem') \
             AND timestamp > 100 AND timestamp < 200",
        )
        .await;
        assert_eq!(
            bounds,
            ScanBounds {
                since: Some(101),
                until: Some(199),
                names: names(&["cpu", "mem"]),
            }
        );
        let between = bounds_for("SELECT * FROM metrics WHERE timestamp BETWEEN 5 AND 9").await;
        assert_eq!((between.since, between.until), (Some(5), Some(9)));
    }

    #[tokio::test]
    async fn bounds_inside_a_subquery_still_reach_the_scan() {
        let bounds = bounds_for(
            "SELECT timestamp FROM (SELECT timestamp, ROW_NUMBER() OVER \
             (PARTITION BY labels ORDER BY timestamp DESC) AS rank FROM metrics \
             WHERE timestamp >= 7) WHERE rank <= 2",
        )
        .await;
        assert_eq!(bounds.since, Some(7));
    }

    #[tokio::test]
    async fn a_disjunction_narrows_to_the_hull_of_its_sides() {
        let bounds = bounds_for(
            "SELECT * FROM metrics WHERE \
             (metric_name = 'cpu' AND timestamp >= 10 AND timestamp <= 20) \
             OR (metric_name = 'mem' AND timestamp >= 30 AND timestamp <= 40)",
        )
        .await;
        assert_eq!(
            bounds,
            ScanBounds {
                since: Some(10),
                until: Some(40),
                names: names(&["cpu", "mem"]),
            }
        );
    }

    /// A disjunction with an unbounded side can want any row, and so must
    /// not narrow anything.
    #[tokio::test]
    async fn disjunctions_and_negations_give_no_bound() {
        for sql in [
            "SELECT * FROM metrics WHERE timestamp >= 100 OR metric_name = 'cpu'",
            "SELECT * FROM metrics WHERE NOT (timestamp >= 100 AND metric_name = 'cpu')",
            "SELECT * FROM metrics WHERE metric_name NOT IN ('cpu')",
            "SELECT * FROM metrics WHERE metric_name <> 'cpu'",
            "SELECT * FROM metrics WHERE timestamp NOT BETWEEN 1 AND 5",
        ] {
            let bounds = bounds_for(sql).await;
            assert_eq!(bounds, ScanBounds::default(), "{sql}");
        }
    }

    #[test]
    fn contradictory_bounds_admit_nothing() {
        let bounds = ScanBounds {
            since: Some(10),
            until: Some(5),
            names: None,
        };
        assert!(!bounds.admits_times(None, None));
        let no_names = ScanBounds {
            names: Some(BTreeSet::new()),
            ..ScanBounds::default()
        };
        assert!(!no_names.admits_names(None, None));
    }

    #[test]
    fn statistics_prune_only_ranges_that_cannot_match() {
        let bounds = ScanBounds {
            since: Some(100),
            until: Some(200),
            names: names(&["cpu"]),
        };
        assert!(bounds.admits_times(Some(50), Some(100)));
        assert!(bounds.admits_times(Some(200), Some(300)));
        assert!(!bounds.admits_times(Some(10), Some(99)));
        assert!(!bounds.admits_times(Some(201), Some(300)));
        assert!(bounds.admits_times(None, None));
        assert!(bounds.admits_names(Some(b"app"), Some(b"mem")));
        assert!(bounds.admits_names(Some(b"cpu"), Some(b"cpu")));
        assert!(!bounds.admits_names(Some(b"disk"), Some(b"mem")));
        assert!(!bounds.admits_names(Some(b"a"), Some(b"cp")));
        assert!(bounds.admits_names(None, Some(b"zzz")));
    }

    /// The buffer alone is a table too: an empty store still answers.
    #[tokio::test]
    async fn a_table_with_no_files_streams_its_buffer() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "timestamp",
            DataType::UInt64,
            false,
        )]));
        let buffer = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(datafusion::arrow::array::UInt64Array::from(vec![
                3, 1, 2,
            ]))],
        )
        .unwrap();
        let ctx = SessionContext::new();
        let table = ParquetTable::new(
            Arc::clone(&schema),
            Vec::new(),
            Some(buffer.clone()),
            None,
            "metrics",
        );
        ctx.register_table("t", Arc::new(table)).unwrap();
        let reference = SessionContext::new();
        reference
            .register_table(
                "t",
                Arc::new(MemTable::try_new(schema, vec![vec![buffer]]).unwrap()),
            )
            .unwrap();
        for sql in [
            "SELECT COUNT(*) AS n FROM t",
            "SELECT timestamp FROM t WHERE timestamp > 1 ORDER BY timestamp",
        ] {
            let ours = ctx.sql(sql).await.unwrap().collect().await.unwrap();
            let theirs = reference.sql(sql).await.unwrap().collect().await.unwrap();
            assert_eq!(
                datafusion::arrow::util::pretty::pretty_format_batches(&ours)
                    .unwrap()
                    .to_string(),
                datafusion::arrow::util::pretty::pretty_format_batches(&theirs)
                    .unwrap()
                    .to_string(),
                "{sql}"
            );
        }
    }
}
