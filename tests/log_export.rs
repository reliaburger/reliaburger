/// Integration tests for Parquet log export and remote query.
///
/// Verifies end-to-end: insert log entries into a LogStore, flush to
/// Parquet, export to a destination directory, then query the exported
/// files via DataFusion ListingTable.
use reliaburger::ketchup::export::{ExportCheckpoint, export_logs};
use reliaburger::ketchup::log_store::LogStore;
use reliaburger::ketchup::remote_query::{query_remote, query_remote_json};
use reliaburger::ketchup::types::LogStream;

/// Export creates correct directory structure and files are queryable.
#[tokio::test]
async fn export_and_query_round_trip() {
    let source_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();

    // Create a LogStore and insert entries for two apps
    let mut store = LogStore::new(source_dir.path().to_path_buf());
    store.append_at(1000, "web", "default", LogStream::Stdout, "web request 1");
    store.append_at(1001, "web", "default", LogStream::Stderr, "web error");
    store.append_at(1002, "api", "prod", LogStream::Stdout, "api started");
    store.append_at(1003, "api", "prod", LogStream::Stdout, "api ready");
    store.flush().await.unwrap();

    // Export to destination
    let mut checkpoint = ExportCheckpoint::default();
    let result = export_logs(
        source_dir.path(),
        dest_dir.path().to_str().unwrap(),
        "node-1",
        &mut checkpoint,
    )
    .unwrap();

    assert_eq!(result.files_exported, 1);
    assert!(result.bytes_written > 0);

    // Verify directory structure
    let export_path = dest_dir.path().join("node-1");
    assert!(export_path.exists());

    // Query the exported files
    let entries = query_remote(
        export_path.to_str().unwrap(),
        "SELECT timestamp, app, namespace, stream, line FROM logs ORDER BY timestamp",
    )
    .await
    .unwrap();

    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0].timestamp, 1000);
    assert_eq!(entries[0].line, "web request 1");
    assert_eq!(entries[1].line, "web error");
    assert_eq!(entries[1].stream, LogStream::Stderr);
    assert_eq!(entries[2].line, "api started");
    assert_eq!(entries[3].line, "api ready");
}

/// SQL filtering works on exported Parquet files.
#[tokio::test]
async fn query_exported_with_filter() {
    let source_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();

    let mut store = LogStore::new(source_dir.path().to_path_buf());
    store.append_at(1, "web", "default", LogStream::Stdout, "INFO ok");
    store.append_at(2, "web", "default", LogStream::Stderr, "ERROR fail");
    store.append_at(3, "api", "default", LogStream::Stdout, "INFO ready");
    store.flush().await.unwrap();

    let mut checkpoint = ExportCheckpoint::default();
    export_logs(
        source_dir.path(),
        dest_dir.path().to_str().unwrap(),
        "node-1",
        &mut checkpoint,
    )
    .unwrap();

    // Filter by app
    let web_entries = query_remote(
        dest_dir.path().join("node-1").to_str().unwrap(),
        "SELECT timestamp, app, namespace, stream, line FROM logs WHERE app = 'web' ORDER BY timestamp",
    )
    .await
    .unwrap();
    assert_eq!(web_entries.len(), 2);

    // Filter by grep pattern
    let error_entries = query_remote(
        dest_dir.path().join("node-1").to_str().unwrap(),
        "SELECT timestamp, app, namespace, stream, line FROM logs WHERE line LIKE '%ERROR%'",
    )
    .await
    .unwrap();
    assert_eq!(error_entries.len(), 1);
    assert_eq!(error_entries[0].line, "ERROR fail");
}

/// Aggregation queries work on exported data.
#[tokio::test]
async fn query_exported_aggregation() {
    let source_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();

    let mut store = LogStore::new(source_dir.path().to_path_buf());
    for i in 0..10 {
        store.append_at(i, "web", "default", LogStream::Stdout, &format!("line {i}"));
    }
    for i in 10..15 {
        store.append_at(i, "api", "default", LogStream::Stdout, &format!("line {i}"));
    }
    store.flush().await.unwrap();

    let mut checkpoint = ExportCheckpoint::default();
    export_logs(
        source_dir.path(),
        dest_dir.path().to_str().unwrap(),
        "node-1",
        &mut checkpoint,
    )
    .unwrap();

    let rows = query_remote_json(
        dest_dir.path().join("node-1").to_str().unwrap(),
        "SELECT app, COUNT(*) as cnt FROM logs GROUP BY app ORDER BY app",
    )
    .await
    .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["app"], "api");
    assert_eq!(rows[0]["cnt"], 5);
    assert_eq!(rows[1]["app"], "web");
    assert_eq!(rows[1]["cnt"], 10);
}

/// Incremental export: second export only picks up new files.
#[tokio::test]
async fn incremental_export() {
    let source_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();

    let mut store = LogStore::new(source_dir.path().to_path_buf());
    store.append_at(1, "web", "default", LogStream::Stdout, "batch 1");
    store.flush().await.unwrap();

    // First export
    let mut checkpoint = ExportCheckpoint::default();
    let r1 = export_logs(
        source_dir.path(),
        dest_dir.path().to_str().unwrap(),
        "node-1",
        &mut checkpoint,
    )
    .unwrap();
    assert_eq!(r1.files_exported, 1);

    // Add more data and flush (creates a new Parquet file)
    store.append_at(2, "web", "default", LogStream::Stdout, "batch 2");
    store.flush().await.unwrap();

    // Second export: only the new file
    let r2 = export_logs(
        source_dir.path(),
        dest_dir.path().to_str().unwrap(),
        "node-1",
        &mut checkpoint,
    )
    .unwrap();
    assert_eq!(r2.files_exported, 1);

    // Both batches queryable
    let entries = query_remote(
        dest_dir.path().join("node-1").to_str().unwrap(),
        "SELECT timestamp, app, namespace, stream, line FROM logs ORDER BY timestamp",
    )
    .await
    .unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].line, "batch 1");
    assert_eq!(entries[1].line, "batch 2");
}
