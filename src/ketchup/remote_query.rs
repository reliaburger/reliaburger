//! Query Parquet log archives via DataFusion.
//!
//! Given a directory of exported Parquet files, registers them as a
//! DataFusion `ListingTable` and executes SQL queries. Works with
//! local filesystem paths and (with the right `object_store` features)
//! S3/GCS URIs.

use std::sync::Arc;

use datafusion::arrow::array::StringArray;
use datafusion::arrow::array::UInt64Array;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion::prelude::*;

use super::log_store::log_schema;
use super::types::{KetchupError, LogEntry, LogStream};

/// Query exported Parquet log files using SQL.
///
/// `source_path` is a directory containing `.parquet` files (e.g.,
/// `/tmp/export/node-1/` or `s3://bucket/logs/node-1/`).
///
/// The SQL query runs against a `logs` table with columns:
/// `timestamp (u64)`, `app (utf8)`, `namespace (utf8)`,
/// `stream (utf8)`, `line (utf8)`.
pub async fn query_remote(source_path: &str, sql: &str) -> Result<Vec<LogEntry>, KetchupError> {
    let ctx = SessionContext::new();

    // Parse the source path as a ListingTableUrl
    let table_url = ListingTableUrl::parse(source_path).map_err(|e| {
        KetchupError::Io(std::io::Error::other(format!("invalid source path: {e}")))
    })?;

    // Configure listing options for Parquet files
    let listing_options =
        ListingOptions::new(Arc::new(ParquetFormat::default())).with_file_extension(".parquet");

    // Create the listing table config with our schema
    let config = ListingTableConfig::new(table_url)
        .with_listing_options(listing_options)
        .with_schema(Arc::new(log_schema()));

    let table = ListingTable::try_new(config).map_err(|e| {
        KetchupError::Io(std::io::Error::other(format!(
            "failed to create listing table: {e}"
        )))
    })?;

    ctx.register_table("logs", Arc::new(table)).map_err(|e| {
        KetchupError::Io(std::io::Error::other(format!(
            "failed to register table: {e}"
        )))
    })?;

    // Execute the query
    let df = ctx
        .sql(sql)
        .await
        .map_err(|e| KetchupError::Io(std::io::Error::other(format!("query failed: {e}"))))?;

    let batches = df.collect().await.map_err(|e| {
        KetchupError::Io(std::io::Error::other(format!(
            "query execution failed: {e}"
        )))
    })?;

    // Parse results into LogEntry
    let mut entries = Vec::new();
    for batch in &batches {
        if batch.num_columns() < 5 {
            continue;
        }
        let timestamps = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| {
                KetchupError::Io(std::io::Error::other("timestamp column type mismatch"))
            })?;
        let _apps = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| KetchupError::Io(std::io::Error::other("app column type mismatch")))?;
        let _namespaces = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| {
                KetchupError::Io(std::io::Error::other("namespace column type mismatch"))
            })?;
        let streams = batch
            .column(3)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| {
                KetchupError::Io(std::io::Error::other("stream column type mismatch"))
            })?;
        let lines = batch
            .column(4)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| KetchupError::Io(std::io::Error::other("line column type mismatch")))?;

        for i in 0..batch.num_rows() {
            let stream = match streams.value(i) {
                "stderr" => LogStream::Stderr,
                _ => LogStream::Stdout,
            };
            entries.push(LogEntry {
                timestamp: timestamps.value(i),
                stream,
                line: lines.value(i).to_string(),
            });
        }
    }

    Ok(entries)
}

/// Query exported Parquet logs and return results as formatted JSON.
///
/// Like `query_remote` but returns arbitrary SQL result columns as
/// JSON values (useful for aggregation queries like COUNT, AVG).
pub async fn query_remote_json(
    source_path: &str,
    sql: &str,
) -> Result<Vec<serde_json::Value>, KetchupError> {
    let ctx = SessionContext::new();

    let table_url = ListingTableUrl::parse(source_path).map_err(|e| {
        KetchupError::Io(std::io::Error::other(format!("invalid source path: {e}")))
    })?;

    let listing_options =
        ListingOptions::new(Arc::new(ParquetFormat::default())).with_file_extension(".parquet");

    let config = ListingTableConfig::new(table_url)
        .with_listing_options(listing_options)
        .with_schema(Arc::new(log_schema()));

    let table = ListingTable::try_new(config).map_err(|e| {
        KetchupError::Io(std::io::Error::other(format!(
            "failed to create listing table: {e}"
        )))
    })?;

    ctx.register_table("logs", Arc::new(table)).map_err(|e| {
        KetchupError::Io(std::io::Error::other(format!(
            "failed to register table: {e}"
        )))
    })?;

    let df = ctx
        .sql(sql)
        .await
        .map_err(|e| KetchupError::Io(std::io::Error::other(format!("query failed: {e}"))))?;

    let batches = df.collect().await.map_err(|e| {
        KetchupError::Io(std::io::Error::other(format!(
            "query execution failed: {e}"
        )))
    })?;

    let mut rows = Vec::new();
    for batch in &batches {
        let schema = batch.schema();
        for i in 0..batch.num_rows() {
            let mut row = serde_json::Map::new();
            for (col_idx, field) in schema.fields().iter().enumerate() {
                let col = batch.column(col_idx);
                let value = if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
                    serde_json::Value::String(arr.value(i).to_string())
                } else if let Some(arr) = col.as_any().downcast_ref::<UInt64Array>() {
                    serde_json::Value::Number(arr.value(i).into())
                } else if let Some(arr) = col
                    .as_any()
                    .downcast_ref::<datafusion::arrow::array::Int64Array>()
                {
                    serde_json::Value::Number(arr.value(i).into())
                } else if let Some(arr) = col
                    .as_any()
                    .downcast_ref::<datafusion::arrow::array::Float64Array>()
                {
                    serde_json::json!(arr.value(i))
                } else {
                    serde_json::Value::String(format!("{:?}", col))
                };
                row.insert(field.name().clone(), value);
            }
            rows.push(serde_json::Value::Object(row));
        }
    }

    Ok(rows)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::ketchup::log_store::LogStore;

    /// Helper: create a LogStore, insert entries, flush to Parquet.
    async fn store_with_entries(
        dir: &Path,
        entries: &[(u64, &str, &str, LogStream, &str)],
    ) -> LogStore {
        let mut store = LogStore::new(dir.to_path_buf());
        for &(ts, app, ns, stream, line) in entries {
            store.append_at(ts, app, ns, stream, line);
        }
        store.flush().await.unwrap();
        store
    }

    #[tokio::test]
    async fn query_local_parquet_files() {
        let dir = tempfile::tempdir().unwrap();
        let _store = store_with_entries(
            dir.path(),
            &[
                (1000, "web", "default", LogStream::Stdout, "hello"),
                (1001, "web", "default", LogStream::Stderr, "error"),
                (1002, "api", "default", LogStream::Stdout, "started"),
            ],
        )
        .await;

        let entries = query_remote(
            dir.path().to_str().unwrap(),
            "SELECT timestamp, app, namespace, stream, line FROM logs ORDER BY timestamp",
        )
        .await
        .unwrap();

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].timestamp, 1000);
        assert_eq!(entries[0].line, "hello");
        assert_eq!(entries[1].stream, LogStream::Stderr);
        assert_eq!(entries[2].line, "started");
    }

    #[tokio::test]
    async fn query_with_where_filter() {
        let dir = tempfile::tempdir().unwrap();
        let _store = store_with_entries(
            dir.path(),
            &[
                (1, "web", "default", LogStream::Stdout, "INFO ok"),
                (2, "web", "default", LogStream::Stderr, "ERROR bad"),
                (3, "api", "default", LogStream::Stdout, "INFO ready"),
            ],
        )
        .await;

        let entries = query_remote(
            dir.path().to_str().unwrap(),
            "SELECT timestamp, app, namespace, stream, line FROM logs WHERE app = 'web' ORDER BY timestamp",
        )
        .await
        .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].line, "INFO ok");
        assert_eq!(entries[1].line, "ERROR bad");
    }

    #[tokio::test]
    async fn query_time_range() {
        let dir = tempfile::tempdir().unwrap();
        let _store = store_with_entries(
            dir.path(),
            &[
                (100, "web", "default", LogStream::Stdout, "early"),
                (200, "web", "default", LogStream::Stdout, "middle"),
                (300, "web", "default", LogStream::Stdout, "late"),
            ],
        )
        .await;

        let entries = query_remote(
            dir.path().to_str().unwrap(),
            "SELECT timestamp, app, namespace, stream, line FROM logs WHERE timestamp >= 150 AND timestamp <= 250 ORDER BY timestamp",
        )
        .await
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].line, "middle");
    }

    #[tokio::test]
    async fn query_empty_directory() {
        let dir = tempfile::tempdir().unwrap();

        let entries = query_remote(
            dir.path().to_str().unwrap(),
            "SELECT timestamp, app, namespace, stream, line FROM logs",
        )
        .await
        .unwrap();

        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn query_json_aggregation() {
        let dir = tempfile::tempdir().unwrap();
        let _store = store_with_entries(
            dir.path(),
            &[
                (1, "web", "default", LogStream::Stdout, "line1"),
                (2, "web", "default", LogStream::Stdout, "line2"),
                (3, "api", "default", LogStream::Stdout, "line3"),
            ],
        )
        .await;

        let rows = query_remote_json(
            dir.path().to_str().unwrap(),
            "SELECT app, COUNT(*) as cnt FROM logs GROUP BY app ORDER BY app",
        )
        .await
        .unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["app"], "api");
        assert_eq!(rows[0]["cnt"], 1);
        assert_eq!(rows[1]["app"], "web");
        assert_eq!(rows[1]["cnt"], 2);
    }
}
