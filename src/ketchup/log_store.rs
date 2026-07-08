//! Arrow/DataFusion-based log store.
//!
//! Logs are buffered in memory, periodically flushed to Parquet, and
//! queryable via DataFusion SQL. Mirrors the MayoStore architecture
//! exactly — same engine for both metrics and logs.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use datafusion::arrow::array::StringArray;
use datafusion::arrow::array::UInt64Array;
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::MemTable;
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

/// Rows per Parquet row group for flushed log files.
///
/// Logs are written in small row groups so that row-group statistics and
/// bloom filters can skip irrelevant groups during archive queries. A
/// flush of a few thousand lines then spans several groups rather than one
/// monolithic block.
const LOG_ROW_GROUP_SIZE: usize = 8192;

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
        .set_max_row_group_size(LOG_ROW_GROUP_SIZE);
    for column in ["app", "namespace"] {
        builder = builder
            .set_column_bloom_filter_enabled(column.into(), true)
            .set_column_bloom_filter_fpp(column.into(), BLOOM_FPP)
            .set_column_bloom_filter_ndv(column.into(), BLOOM_NDV);
    }
    builder.build()
}

/// Arrow schema for the logs table.
pub fn log_schema() -> Schema {
    Schema::new(vec![
        Field::new("timestamp", DataType::UInt64, false),
        Field::new("app", DataType::Utf8, false),
        Field::new("namespace", DataType::Utf8, false),
        Field::new("stream", DataType::Utf8, false),
        Field::new("line", DataType::Utf8, false),
    ])
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

/// A buffered log entry waiting to be flushed.
struct BufferedLogEntry {
    timestamp: u64,
    app: String,
    namespace: String,
    stream: String,
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
}

impl LogStore {
    /// Open (or create) a log store writing Parquet to `data_dir`.
    ///
    /// Existing `logs_NNNNNN.parquet` files remain queryable and the flush
    /// counter resumes past the highest one.
    pub fn new(data_dir: PathBuf) -> Self {
        let flush_counter = next_flush_counter(&data_dir, "logs");
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

    /// Append a log line.
    pub fn append(&mut self, app: &str, namespace: &str, stream: LogStream, line: &str) {
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.append_at(timestamp, app, namespace, stream, line);
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
        let stream_str = match stream {
            LogStream::Stdout => "stdout",
            LogStream::Stderr => "stderr",
        };
        self.buffer.push(BufferedLogEntry {
            timestamp,
            app: app.to_string(),
            namespace: namespace.to_string(),
            stream: stream_str.to_string(),
            line: line.to_string(),
        });
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
        let streams: Vec<&str> = self.buffer.iter().map(|e| e.stream.as_str()).collect();
        let lines: Vec<&str> = self.buffer.iter().map(|e| e.line.as_str()).collect();

        let batch = RecordBatch::try_new(
            Arc::new(log_schema()),
            vec![
                Arc::new(UInt64Array::from(timestamps)),
                Arc::new(StringArray::from(apps)),
                Arc::new(StringArray::from(namespaces)),
                Arc::new(StringArray::from(streams)),
                Arc::new(StringArray::from(lines)),
            ],
        )
        .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;

        Ok(Some(batch))
    }

    /// Flush the buffer to Parquet.
    pub async fn flush(&mut self) -> Result<(), KetchupError> {
        let batch = self.buffer_to_batch()?;
        let Some(batch) = batch else {
            return Ok(());
        };

        std::fs::create_dir_all(&self.data_dir)?;
        let filename = format!("logs_{:06}.parquet", self.flush_counter);
        let path = self.data_dir.join(filename);

        let file = std::fs::File::create(&path)?;
        let mut writer =
            ArrowWriter::try_new(file, Arc::new(log_schema()), Some(log_writer_properties()))
                .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        writer
            .write(&batch)
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        writer
            .close()
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;

        // Durable on disk now; drop from memory. Queries read it back from the
        // Parquet directory (see `session`).
        self.buffer.clear();
        self.flush_counter += 1;
        Ok(())
    }

    /// Build a DataFusion session exposing a `logs` table over the on-disk
    /// Parquet directory unioned with the unflushed buffer.
    async fn session(&self) -> Result<SessionContext, KetchupError> {
        // Read Parquet string columns as `Utf8`, not `Utf8View`, so on-disk
        // batches share the canonical `log_schema` with the in-memory buffer.
        let config = SessionConfig::new().set_bool(
            "datafusion.execution.parquet.schema_force_view_types",
            false,
        );
        let ctx = SessionContext::new_with_config(config);
        let schema = Arc::new(log_schema());

        let mut all_batches = self.read_disk_batches(&ctx).await?;
        if let Some(buffer_batch) = self.buffer_to_batch()? {
            all_batches.push(buffer_batch);
        }
        if all_batches.is_empty() {
            all_batches.push(RecordBatch::new_empty(schema.clone()));
        }

        let table = MemTable::try_new(schema, vec![all_batches])
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        ctx.register_table("logs", Arc::new(table))
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        Ok(ctx)
    }

    /// Read every `logs_*.parquet` file into RecordBatches normalised to the
    /// canonical `log_schema`.
    async fn read_disk_batches(
        &self,
        ctx: &SessionContext,
    ) -> Result<Vec<RecordBatch>, KetchupError> {
        if !dir_has_parquet(&self.data_dir) {
            return Ok(Vec::new());
        }
        let dir = format!("{}/", self.data_dir.display());
        ctx.register_parquet("logs_disk", &dir, ParquetReadOptions::default())
            .await
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        let batches = ctx
            .sql("SELECT timestamp, app, namespace, stream, line FROM logs_disk")
            .await
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?
            .collect()
            .await
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;

        let schema = Arc::new(log_schema());
        let mut normalised = Vec::with_capacity(batches.len());
        for batch in batches {
            normalised.push(
                RecordBatch::try_new(schema.clone(), batch.columns().to_vec())
                    .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?,
            );
        }
        Ok(normalised)
    }

    /// Query logs using SQL and return raw JSON rows.
    ///
    /// Unlike `query_sql()`, this returns arbitrary columns as JSON
    /// objects, so `SELECT timestamp, line FROM logs` works without
    /// requiring all 5 columns.
    pub async fn query_sql_json(&self, sql: &str) -> Result<Vec<serde_json::Value>, KetchupError> {
        let ctx = self.session().await?;
        let df = ctx
            .sql(sql)
            .await
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;

        let batches = df
            .collect()
            .await
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;

        let mut results = Vec::new();
        for batch in &batches {
            let schema = batch.schema();
            for row in 0..batch.num_rows() {
                let mut obj = serde_json::Map::new();
                for (col_idx, field) in schema.fields().iter().enumerate() {
                    let col = batch.column(col_idx);
                    let value = if let Some(arr) = col.as_any().downcast_ref::<UInt64Array>() {
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

        Ok(results)
    }

    /// Query logs using SQL, returning structured LogEntry results.
    /// Requires the query to return all 5 columns in schema order.
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

        let mut results = Vec::new();
        for batch in &batches {
            if batch.num_columns() < 5 {
                continue;
            }
            let timestamps = batch.column(0).as_any().downcast_ref::<UInt64Array>();
            let apps = batch.column(1).as_any().downcast_ref::<StringArray>();
            let _namespaces = batch.column(2).as_any().downcast_ref::<StringArray>();
            let streams = batch.column(3).as_any().downcast_ref::<StringArray>();
            let lines = batch.column(4).as_any().downcast_ref::<StringArray>();

            if let (Some(ts), Some(_app), Some(st), Some(ln)) = (timestamps, apps, streams, lines) {
                for i in 0..batch.num_rows() {
                    let stream = match st.value(i) {
                        "stderr" => LogStream::Stderr,
                        _ => LogStream::Stdout,
                    };
                    results.push(LogEntry {
                        timestamp: ts.value(i),
                        stream,
                        line: ln.value(i).to_string(),
                    });
                }
            }
        }

        Ok(results)
    }

    /// Convenience: query by app, time range, grep pattern, and limit.
    pub async fn query(
        &self,
        app: &str,
        namespace: &str,
        start: Option<u64>,
        end: Option<u64>,
        grep: Option<&str>,
        limit: Option<usize>,
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
        let limit_clause = limit.map(|l| format!(" LIMIT {l}")).unwrap_or_default();

        let sql = format!(
            "SELECT timestamp, app, namespace, stream, line FROM logs \
             WHERE {where_clause} ORDER BY timestamp{limit_clause}"
        );
        self.query_sql(&sql).await
    }

    /// List all distinct (app, namespace) pairs in the store.
    pub async fn query_apps(&self) -> Result<Vec<(String, String)>, KetchupError> {
        let ctx = self.session().await?;
        let df = ctx
            .sql("SELECT DISTINCT app, namespace FROM logs ORDER BY app, namespace")
            .await
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;

        let batches = df
            .collect()
            .await
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;

        let mut results = Vec::new();
        for batch in &batches {
            let apps = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| {
                    KetchupError::Io(std::io::Error::other("app column type mismatch"))
                })?;
            let namespaces = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| {
                    KetchupError::Io(std::io::Error::other("namespace column type mismatch"))
                })?;
            for i in 0..batch.num_rows() {
                results.push((apps.value(i).to_string(), namespaces.value(i).to_string()));
            }
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (LogStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = LogStore::new(dir.path().to_path_buf());
        (store, dir)
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
    async fn schema_has_five_columns() {
        let schema = log_schema();
        assert_eq!(schema.fields().len(), 5);
        assert_eq!(schema.field(0).name(), "timestamp");
        assert_eq!(schema.field(1).name(), "app");
        assert_eq!(schema.field(2).name(), "namespace");
        assert_eq!(schema.field(3).name(), "stream");
        assert_eq!(schema.field(4).name(), "line");
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

    /// A batch of semi-realistic, semi-repetitive log lines.
    fn log_batch(rows: usize) -> (RecordBatch, usize) {
        let timestamps: Vec<u64> = (0..rows as u64).collect();
        let apps: Vec<&str> = (0..rows)
            .map(|i| if i % 2 == 0 { "web" } else { "api" })
            .collect();
        let namespaces: Vec<&str> = vec!["default"; rows];
        let streams: Vec<&str> = vec!["stdout"; rows];
        let lines: Vec<String> = (0..rows)
            .map(|i| format!("GET /api/v1/users/{} 200 OK in {}ms", i % 100, i % 50))
            .collect();
        // Raw-text size: roughly what the flat .log file would hold.
        let raw_text_bytes: usize = lines
            .iter()
            .zip(&timestamps)
            .map(|(l, ts)| l.len() + ts.to_string().len() + 3) // "{ts} O {line}\n"
            .sum();
        let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let batch = RecordBatch::try_new(
            Arc::new(log_schema()),
            vec![
                Arc::new(UInt64Array::from(timestamps)),
                Arc::new(StringArray::from(apps)),
                Arc::new(StringArray::from(namespaces)),
                Arc::new(StringArray::from(streams)),
                Arc::new(StringArray::from(line_refs)),
            ],
        )
        .unwrap();
        (batch, raw_text_bytes)
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
        let batch = RecordBatch::try_new(
            Arc::new(log_schema()),
            vec![
                Arc::new(UInt64Array::from((0..n as u64).collect::<Vec<_>>())),
                Arc::new(StringArray::from(app_refs)),
                Arc::new(StringArray::from(vec!["default"; n])),
                Arc::new(StringArray::from(vec!["stdout"; n])),
                Arc::new(StringArray::from(vec!["x"; n])),
            ],
        )
        .unwrap();
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
