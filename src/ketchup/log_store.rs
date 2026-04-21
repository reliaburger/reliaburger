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
use datafusion::prelude::*;

use super::types::{KetchupError, LogEntry, LogStream};

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
/// Same architecture as MayoStore: buffer in memory, flush to Parquet
/// periodically, query via DataFusion SQL. The unflushed buffer is
/// included in every query so there are no blind spots.
pub struct LogStore {
    buffer: Vec<BufferedLogEntry>,
    batches: Vec<RecordBatch>,
    data_dir: PathBuf,
    flush_counter: u64,
}

impl LogStore {
    /// Create a new log store writing Parquet to `data_dir`.
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            buffer: Vec::new(),
            batches: Vec::new(),
            data_dir,
            flush_counter: 0,
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
        let mut writer = ArrowWriter::try_new(file, Arc::new(log_schema()), None)
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        writer
            .write(&batch)
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        writer
            .close()
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;

        self.batches.push(batch);
        self.buffer.clear();
        self.flush_counter += 1;
        Ok(())
    }

    /// Build a DataFusion session with all data (flushed + unflushed buffer).
    async fn session(&self) -> Result<SessionContext, KetchupError> {
        let ctx = SessionContext::new();

        let mut all_batches = self.batches.clone();
        if let Some(buffer_batch) = self.buffer_to_batch()? {
            all_batches.push(buffer_batch);
        }

        if all_batches.is_empty() {
            let empty = RecordBatch::new_empty(Arc::new(log_schema()));
            let table = MemTable::try_new(Arc::new(log_schema()), vec![vec![empty]])
                .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
            ctx.register_table("logs", Arc::new(table))
                .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        } else {
            let table = MemTable::try_new(Arc::new(log_schema()), vec![all_batches])
                .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
            ctx.register_table("logs", Arc::new(table))
                .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        }
        Ok(ctx)
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
}
