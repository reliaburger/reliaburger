/// Integration tests for cross-node log queries.
///
/// Spins up lightweight axum servers (one per simulated node) that
/// return known log entries, then uses `fan_out_query` to verify
/// merge-sort and deduplication behaviour.
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query};
use axum::response::Json;
use axum::routing::get;
use serde::Deserialize;
use tokio::sync::RwLock;

use reliaburger::ketchup::log_store::LogStore;
use reliaburger::ketchup::query::fan_out_query;
use reliaburger::ketchup::types::{LogQuery, LogStream};

/// Query params matching the /v1/logs/entries endpoint.
#[derive(Deserialize)]
struct LogsParams {
    start: Option<u64>,
    end: Option<u64>,
    grep: Option<String>,
    tail: Option<usize>,
}

/// Build a test router that serves log entries from a LogStore.
fn test_router(store: Arc<RwLock<LogStore>>) -> Router {
    Router::new().route(
        "/v1/logs/entries/{app}/{namespace}",
        get(
            move |path: Path<(String, String)>, query: Query<LogsParams>| {
                let store = store.clone();
                async move {
                    let (app, namespace) = path.0;
                    let s = store.read().await;
                    let entries = s
                        .query(
                            &app,
                            &namespace,
                            query.start,
                            query.end,
                            query.grep.as_deref(),
                            query.tail,
                        )
                        .await
                        .unwrap_or_default();
                    Json(entries)
                }
            },
        ),
    )
}

/// Start a test server on an ephemeral port and return its base URL.
async fn start_server(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

/// Create a LogStore with entries at the given timestamps.
async fn store_with_entries(
    timestamps: &[u64],
    prefix: &str,
) -> (Arc<RwLock<LogStore>>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = LogStore::new(dir.path().to_path_buf());
    for &ts in timestamps {
        store.append_at(
            ts,
            "web",
            "default",
            LogStream::Stdout,
            &format!("{prefix} ts={ts}"),
        );
    }
    store.flush().await.unwrap();
    (Arc::new(RwLock::new(store)), dir)
}

/// 3 nodes with disjoint timestamps. All lines should appear in the
/// merged result, sorted by timestamp.
#[tokio::test]
async fn three_nodes_merge_sorted() {
    let (s1, _d1) = store_with_entries(&[1, 4, 7], "node1").await;
    let (s2, _d2) = store_with_entries(&[2, 5, 8], "node2").await;
    let (s3, _d3) = store_with_entries(&[3, 6, 9], "node3").await;

    let url1 = start_server(test_router(s1)).await;
    let url2 = start_server(test_router(s2)).await;
    let url3 = start_server(test_router(s3)).await;

    let query = LogQuery {
        app: "web".to_string(),
        namespace: "default".to_string(),
        ..Default::default()
    };

    let client = reqwest::Client::new();
    let timeout = std::time::Duration::from_secs(5);
    let result = fan_out_query(&query, &[url1, url2, url3], &client, timeout)
        .await
        .unwrap();

    // All 9 entries should be present, sorted by timestamp
    assert_eq!(result.len(), 9);
    for i in 0..9 {
        assert_eq!(result[i].timestamp, (i + 1) as u64);
    }
}

/// Two nodes with overlapping entries. Duplicates (same timestamp + line)
/// should be removed.
#[tokio::test]
async fn deduplicates_overlapping_entries() {
    let dir1 = tempfile::tempdir().unwrap();
    let mut store1 = LogStore::new(dir1.path().to_path_buf());
    store1.append_at(1, "web", "default", LogStream::Stdout, "unique to node1");
    store1.append_at(2, "web", "default", LogStream::Stdout, "shared line");
    store1.flush().await.unwrap();

    let dir2 = tempfile::tempdir().unwrap();
    let mut store2 = LogStore::new(dir2.path().to_path_buf());
    store2.append_at(2, "web", "default", LogStream::Stdout, "shared line");
    store2.append_at(3, "web", "default", LogStream::Stdout, "unique to node2");
    store2.flush().await.unwrap();

    let s1 = Arc::new(RwLock::new(store1));
    let s2 = Arc::new(RwLock::new(store2));

    let url1 = start_server(test_router(s1)).await;
    let url2 = start_server(test_router(s2)).await;

    let query = LogQuery {
        app: "web".to_string(),
        namespace: "default".to_string(),
        ..Default::default()
    };

    let client = reqwest::Client::new();
    let result = fan_out_query(
        &query,
        &[url1, url2],
        &client,
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap();

    // "shared line" at ts=2 should appear only once
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].line, "unique to node1");
    assert_eq!(result[1].line, "shared line");
    assert_eq!(result[2].line, "unique to node2");
}

/// Grep filter is applied per-node before merge.
#[tokio::test]
async fn grep_filter_across_nodes() {
    let dir1 = tempfile::tempdir().unwrap();
    let mut store1 = LogStore::new(dir1.path().to_path_buf());
    store1.append_at(1, "web", "default", LogStream::Stdout, "INFO starting");
    store1.append_at(2, "web", "default", LogStream::Stderr, "ERROR failed");
    store1.flush().await.unwrap();

    let dir2 = tempfile::tempdir().unwrap();
    let mut store2 = LogStore::new(dir2.path().to_path_buf());
    store2.append_at(3, "web", "default", LogStream::Stdout, "INFO ready");
    store2.append_at(4, "web", "default", LogStream::Stderr, "ERROR timeout");
    store2.flush().await.unwrap();

    let s1 = Arc::new(RwLock::new(store1));
    let s2 = Arc::new(RwLock::new(store2));

    let url1 = start_server(test_router(s1)).await;
    let url2 = start_server(test_router(s2)).await;

    let query = LogQuery {
        app: "web".to_string(),
        namespace: "default".to_string(),
        grep: Some("ERROR".to_string()),
        ..Default::default()
    };

    let client = reqwest::Client::new();
    let result = fan_out_query(
        &query,
        &[url1, url2],
        &client,
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap();

    // Only ERROR lines from both nodes
    assert_eq!(result.len(), 2);
    assert!(result[0].line.contains("ERROR"));
    assert!(result[1].line.contains("ERROR"));
    assert_eq!(result[0].timestamp, 2);
    assert_eq!(result[1].timestamp, 4);
}

/// One node is unreachable. Results from the available node are
/// still returned (graceful degradation).
#[tokio::test]
async fn partial_results_when_node_unreachable() {
    let (s1, _d1) = store_with_entries(&[1, 2, 3], "node1").await;
    let url1 = start_server(test_router(s1)).await;

    // node2 URL points to a port nothing is listening on
    let url2 = "http://127.0.0.1:1".to_string();

    let query = LogQuery {
        app: "web".to_string(),
        namespace: "default".to_string(),
        ..Default::default()
    };

    let client = reqwest::Client::new();
    let result = fan_out_query(
        &query,
        &[url1, url2],
        &client,
        std::time::Duration::from_secs(2),
    )
    .await
    .unwrap();

    // Only entries from node1 (node2 silently returns empty)
    assert_eq!(result.len(), 3);
    assert!(result[0].line.contains("node1"));
}

/// Time range filtering works across nodes.
#[tokio::test]
async fn time_range_filter_across_nodes() {
    let (s1, _d1) = store_with_entries(&[100, 200, 300], "node1").await;
    let (s2, _d2) = store_with_entries(&[150, 250, 350], "node2").await;

    let url1 = start_server(test_router(s1)).await;
    let url2 = start_server(test_router(s2)).await;

    let query = LogQuery {
        app: "web".to_string(),
        namespace: "default".to_string(),
        start: Some(150),
        end: Some(250),
        ..Default::default()
    };

    let client = reqwest::Client::new();
    let result = fan_out_query(
        &query,
        &[url1, url2],
        &client,
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap();

    // node1: 200 in range. node2: 150, 250 in range.
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].timestamp, 150);
    assert_eq!(result[1].timestamp, 200);
    assert_eq!(result[2].timestamp, 250);
}
