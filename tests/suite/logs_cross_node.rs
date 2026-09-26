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
use reliaburger::ketchup::types::{LogEntry, LogQuery, LogStream};

/// Pair each server URL with a stable node id, as the API layer does.
fn nodes(urls: &[String]) -> Vec<(String, String)> {
    urls.iter()
        .enumerate()
        .map(|(i, u)| (format!("node{}", i + 1), u.clone()))
        .collect()
}

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
                        .expect("test log-store query failed");
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

/// V02 soak regression: an app that moved away from node 1 and came back
/// after a whole-cluster restart. Node 1 holds its lines from half an hour
/// ago, node 2 holds everything since. Asking only where the app is placed
/// now (node 1) returned the half-hour-old lines as the tail.
#[tokio::test]
async fn tail_after_an_app_moves_back_includes_the_nodes_it_ran_on_meanwhile() {
    let (old_home, _dir1) = store_with_entries(&[100, 101, 102], "INCR").await;
    let (meanwhile, _dir2) = store_with_entries(&[200, 201, 202], "INCR").await;
    let url1 = start_server(test_router(old_home)).await;
    let url2 = start_server(test_router(meanwhile)).await;
    let members = nodes(&[url1, url2]);

    let targets = reliaburger::ketchup::query::query_targets(&["node1".to_string()], &members);
    let query = LogQuery {
        app: "web".to_string(),
        namespace: "default".to_string(),
        tail: Some(2),
        ..Default::default()
    };
    let result = fan_out_query(
        &query,
        &targets.reachable,
        &reqwest::Client::new(),
        std::time::Duration::from_secs(5),
        None,
    )
    .await
    .unwrap();

    let mut entries = result.entries;
    let newest = entries.split_off(entries.len().saturating_sub(2));
    let timestamps: Vec<u64> = newest.iter().map(|e| e.timestamp).collect();
    assert_eq!(timestamps, vec![201, 202]);
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
    let result = fan_out_query(&query, &nodes(&[url1, url2, url3]), &client, timeout, None)
        .await
        .unwrap();

    // All 9 entries should be present, sorted by timestamp
    assert!(result.failures.is_empty());
    assert_eq!(result.entries.len(), 9);
    for (i, entry) in result.entries.iter().enumerate() {
        assert_eq!(entry.timestamp, (i + 1) as u64);
    }
}

/// An app that prints the same line twice printed two lines, and an identical
/// line from a DIFFERENT replica is a third event. Only a row one node reports
/// twice (same sequence, a retransmit) is a duplicate; see
/// `ketchup::query::tests::cross_source_duplicates_from_same_node_dedup`.
#[tokio::test]
async fn repeated_lines_and_cross_replica_events_all_survive() {
    let dir1 = tempfile::tempdir().unwrap();
    let mut store1 = LogStore::new(dir1.path().to_path_buf());
    store1.append_at(1, "web", "default", LogStream::Stdout, "unique to node1");
    // The same node prints the shared line twice in one second: two events.
    store1.append_at(2, "web", "default", LogStream::Stdout, "shared line");
    store1.append_at(2, "web", "default", LogStream::Stdout, "shared line");
    store1.flush().await.unwrap();

    let dir2 = tempfile::tempdir().unwrap();
    let mut store2 = LogStore::new(dir2.path().to_path_buf());
    // A DIFFERENT replica logs an identical line at the same instant: distinct.
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
        &nodes(&[url1, url2]),
        &client,
        std::time::Duration::from_secs(5),
        None,
    )
    .await
    .unwrap();

    // unique1, shared(node1) twice, shared(node2), unique2 = 5.
    let shared = result
        .entries
        .iter()
        .filter(|e| e.line == "shared line")
        .count();
    assert_eq!(shared, 3, "every printed line should survive");
    assert_eq!(result.entries.len(), 5);
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
        &nodes(&[url1, url2]),
        &client,
        std::time::Duration::from_secs(5),
        None,
    )
    .await
    .unwrap();

    // Only ERROR lines from both nodes
    assert_eq!(result.entries.len(), 2);
    assert!(result.entries[0].line.contains("ERROR"));
    assert!(result.entries[1].line.contains("ERROR"));
    assert_eq!(result.entries[0].timestamp, 2);
    assert_eq!(result.entries[1].timestamp, 4);
}

/// One node is unreachable. Results from the available node are still
/// returned, AND the unreachable node is reported as a failure — not folded
/// into a silent empty success (OBS6).
#[tokio::test]
async fn partial_results_when_node_unreachable() {
    // This case tests partial transport failure, not cold DataFusion planning
    // under its two-second request budget. Other cases exercise real storage.
    let entries: Vec<_> = (1..=3)
        .map(|timestamp| LogEntry {
            timestamp,
            sequence: timestamp,
            instance: None,
            stream: LogStream::Stdout,
            line: format!("node1 ts={timestamp}"),
        })
        .collect();
    let router = Router::new().route(
        "/v1/logs/entries/{app}/{namespace}",
        get(move || {
            let entries = entries.clone();
            async move { Json(entries) }
        }),
    );
    let url1 = start_server(router).await;

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
        &nodes(&[url1, url2]),
        &client,
        std::time::Duration::from_secs(2),
        None,
    )
    .await
    .unwrap();

    // Entries from node1, and node2 is a reported partial failure.
    assert_eq!(
        result.entries.len(),
        3,
        "node failures: {:?}",
        result.failures
    );
    assert!(result.entries[0].line.contains("node1"));
    assert_eq!(result.failures.len(), 1);
    assert_eq!(result.failures[0].node_id, "node2");
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
        &nodes(&[url1, url2]),
        &client,
        std::time::Duration::from_secs(5),
        None,
    )
    .await
    .unwrap();

    // node1: 200 in range. node2: 150, 250 in range.
    assert_eq!(result.entries.len(), 3);
    assert_eq!(result.entries[0].timestamp, 150);
    assert_eq!(result.entries[1].timestamp, 200);
    assert_eq!(result.entries[2].timestamp, 250);
}
