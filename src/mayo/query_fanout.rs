//! Cross-node metrics query coordination.
//!
//! Fans out metrics queries to council aggregators (cluster-wide) or
//! individual nodes (single-app), then merges results. Follows the
//! same pattern as `ketchup::query::fan_out_query`.

use std::collections::BTreeMap;
use std::time::Duration;

use super::rollup::{
    MetricsQuery, MetricsQueryResult, MetricsQueryRow, OwnedRollupRow, QueryWarning,
};

/// The response header a council member's `/v1/metrics/rollup/owned` sets
/// when its answer continues in another page: the `start` to ask for next
/// (#2451).
pub const NEXT_START_HEADER: &str = "x-reliaburger-next-start";

/// Merge metrics results from multiple sources, sorted by timestamp.
///
/// Deduplicates entries with the same (timestamp, metric_name, labels).
/// Use this for single-app queries where each node reports its own data.
pub fn merge_metrics_results(mut sources: Vec<Vec<MetricsQueryRow>>) -> Vec<MetricsQueryRow> {
    let mut all: Vec<MetricsQueryRow> = sources.drain(..).flatten().collect();
    // Sorting on the whole identity, not the timestamp alone, puts equal
    // rows next to each other: `dedup_by` only removes neighbours (#2477).
    all.sort_by(|a, b| {
        (a.timestamp, &a.metric_name, &a.labels).cmp(&(b.timestamp, &b.metric_name, &b.labels))
    });

    all.dedup_by(|a, b| {
        a.timestamp == b.timestamp && a.metric_name == b.metric_name && a.labels == b.labels
    });

    all
}

/// Merge cluster-wide rollup results by summing partial aggregates.
///
/// Each council member's rollup query returns its own partial sum.
/// This function combines them by summing values for the same
/// (timestamp, metric_name, labels) key. Use this for cluster-wide
/// queries where each council aggregator holds a subset of nodes.
pub fn merge_cluster_results(mut sources: Vec<Vec<MetricsQueryRow>>) -> Vec<MetricsQueryRow> {
    // Group by (timestamp, metric_name, labels) and sum values
    let mut sums: BTreeMap<(u64, String, String), f64> = BTreeMap::new();
    for source in sources.drain(..) {
        for row in source {
            let key = (row.timestamp, row.metric_name, row.labels);
            *sums.entry(key).or_default() += row.value;
        }
    }

    sums.into_iter()
        .map(|((ts, name, labels), value)| MetricsQueryRow {
            timestamp: ts,
            metric_name: name,
            labels,
            value,
        })
        .collect()
}

/// Deduplicate immutable worker/minute/series contributions before summing.
/// Conflicting copies are omitted and marked unavailable rather than selected arbitrarily.
pub fn merge_owned_rollups(sources: Vec<Vec<OwnedRollupRow>>) -> MetricsQueryResult {
    let mut contributions: BTreeMap<(String, u64, String, String), Option<f64>> = BTreeMap::new();
    for owned in sources.into_iter().flatten() {
        let row = owned.row;
        let key = (owned.node_id, row.timestamp, row.metric_name, row.labels);
        contributions
            .entry(key)
            .and_modify(|value| {
                if *value != Some(row.value) {
                    *value = None;
                }
            })
            .or_insert(Some(row.value));
    }
    let mut rows = Vec::new();
    let mut warnings = Vec::new();
    for ((node_id, timestamp, metric_name, labels), value) in contributions {
        match value {
            Some(value) => rows.push(MetricsQueryRow {
                timestamp,
                metric_name,
                labels,
                value,
            }),
            None => warnings.push(QueryWarning::DataUnavailable {
                node_id,
                from: timestamp,
                to: timestamp,
            }),
        }
    }
    MetricsQueryResult {
        data: merge_cluster_results(vec![rows]),
        warnings,
    }
}

/// Fan out a cluster-wide metrics query to all council aggregators.
///
/// Each `council_url` should be a base URL like `http://10.0.1.5:9117`.
/// The query is sent to `GET /v1/metrics/rollup/owned` on each council member.
/// Results are merged by timestamp. Unresponsive nodes produce warnings.
pub async fn fan_out_cluster_query(
    query: &MetricsQuery,
    council_urls: &[String],
    client: &reqwest::Client,
    timeout: Duration,
    service_token: Option<&str>,
) -> MetricsQueryResult {
    let (data_sources, warnings) = fan_out_to_urls(
        query,
        council_urls,
        "/v1/metrics/rollup/owned",
        client,
        timeout,
        service_token,
    )
    .await;

    let mut result = merge_owned_rollups(data_sources.into_iter().map(|(_, rows)| rows).collect());
    result.warnings.extend(warnings);
    result
}

/// Fan out a single-app metrics query to nodes running that app.
///
/// Each `node_url` should be a base URL like `http://10.0.1.5:9117`.
/// The query is sent to `GET /v1/metrics` on each target node.
pub async fn fan_out_app_query(
    query: &MetricsQuery,
    node_urls: &[String],
    client: &reqwest::Client,
    timeout: Duration,
    service_token: Option<&str>,
) -> MetricsQueryResult {
    let (data_sources, warnings) = fan_out_to_urls(
        query,
        node_urls,
        "/v1/metrics",
        client,
        timeout,
        service_token,
    )
    .await;

    merge_app_answers(data_sources, warnings)
}

/// Merge the per-node answers of a single-app query, `(node URL, rows)`
/// each, keeping `warnings`.
///
/// A node answers with at most its newest
/// [`APP_QUERY_ROW_LIMIT`](super::store::APP_QUERY_ROW_LIMIT) rows, so an
/// answer of exactly that many may have lost older ones: it adds a
/// [`QueryWarning::Truncated`] naming the node (#2477).
pub fn merge_app_answers(
    sources: Vec<(String, Vec<MetricsQueryRow>)>,
    mut warnings: Vec<QueryWarning>,
) -> MetricsQueryResult {
    warnings.extend(
        sources
            .iter()
            .filter_map(|(node_id, rows)| truncation_warning(node_id, rows.len())),
    );
    MetricsQueryResult {
        data: merge_metrics_results(sources.into_iter().map(|(_, rows)| rows).collect()),
        warnings,
    }
}

/// A [`QueryWarning::Truncated`] for `node_id` when its answer of `rows`
/// rows reached the per-node limit. The leaf query ends in `LIMIT`, so a
/// full answer means more rows may exist.
pub fn truncation_warning(node_id: &str, rows: usize) -> Option<QueryWarning> {
    let limit = super::store::APP_QUERY_ROW_LIMIT;
    (rows >= limit).then(|| QueryWarning::Truncated {
        node_id: node_id.to_string(),
        limit,
    })
}

/// Internal helper: fan out a query to a set of URLs, collect results.
async fn fan_out_to_urls<T: serde::de::DeserializeOwned + Send + 'static>(
    query: &MetricsQuery,
    urls: &[String],
    path: &str,
    client: &reqwest::Client,
    timeout: Duration,
    service_token: Option<&str>,
) -> (Vec<(String, Vec<T>)>, Vec<QueryWarning>) {
    let mut handles = Vec::new();

    for url in urls {
        let base = url.clone();
        let endpoint = format!("{base}{path}");
        let metric_name = query.metric_name.clone();
        let app = query.app.clone();
        let per_series = query.per_series;
        let start = query.start;
        let end = query.end;
        let client = client.clone();
        let node_url = url.clone();
        let token = service_token.map(str::to_string);

        handles.push(tokio::spawn(async move {
            let mut params = vec![("end", end.to_string())];
            if let Some(name) = metric_name {
                params.push(("name", name));
            }
            if let Some(app) = app {
                params.push(("app", app));
            }
            if let Some(per_series) = per_series {
                params.push(("per_series", per_series.to_string()));
            }
            // The whole answer, every page and its body, inside one wait: a
            // peer can send headers then stall its body.
            let pages = read_pages::<T>(&client, &endpoint, token.as_deref(), &params, start);
            match tokio::time::timeout(timeout, pages).await {
                Ok(Ok(rows)) => Ok((node_url, rows)),
                Ok(Err(MemberFailure::Refused(reason))) => Err(QueryWarning::QueryRefused {
                    node_id: node_url,
                    reason,
                }),
                Ok(Err(MemberFailure::Unresponsive)) | Err(_) => {
                    Err(QueryWarning::NodeUnresponsive { node_id: node_url })
                }
            }
        }));
    }

    let mut sources = Vec::new();
    let mut warnings = Vec::new();

    for handle in handles {
        match handle.await {
            Ok(Ok(answer)) => sources.push(answer),
            Ok(Err(warning)) => warnings.push(warning),
            Err(_join_err) => {
                // Task panicked — treat as unresponsive
                warnings.push(QueryWarning::NodeUnresponsive {
                    node_id: "unknown".to_string(),
                });
            }
        }
    }

    (sources, warnings)
}

/// Why one member contributed nothing.
enum MemberFailure {
    /// No usable answer: connection, timeout or an unreadable body.
    Unresponsive,
    /// It answered, refusing the query, with this reason.
    Refused(String),
}

/// Ask `endpoint` for rows from `start`, following [`NEXT_START_HEADER`]
/// page by page until the member says there are no more (#2451). Stops
/// with a refusal past [`MAX_OWNED_ROWS`](super::rollup_store::MAX_OWNED_ROWS)
/// rows, which bounds what one member can make the entry node hold.
async fn read_pages<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    endpoint: &str,
    token: Option<&str>,
    params: &[(&str, String)],
    start: u64,
) -> Result<Vec<T>, MemberFailure> {
    let mut rows = Vec::new();
    let mut cursor = start;
    loop {
        let response = crate::sesame::auth::bearer_get(client, endpoint, token)
            .query(params)
            .query(&[("start", cursor)])
            .send()
            .await
            .map_err(|_| MemberFailure::Unresponsive)?;
        if !response.status().is_success() {
            return Err(MemberFailure::Refused(refusal_reason(response).await));
        }
        let next = response
            .headers()
            .get(NEXT_START_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let page = response
            .json::<Vec<T>>()
            .await
            .map_err(|_| MemberFailure::Unresponsive)?;
        rows.extend(page);
        if rows.len() > super::rollup_store::MAX_OWNED_ROWS {
            return Err(MemberFailure::Refused(
                super::rollup_store::too_many_contributions().to_string(),
            ));
        }
        match next {
            // A cursor that doesn't advance would ask for the same page for
            // ever.
            Some(next) if next > cursor => cursor = next,
            _ => return Ok(rows),
        }
    }
}

/// What a member's error answer says: the `error` field of a JSON body, or
/// the body itself, cut short, or the status when the body is empty.
async fn refusal_reason(response: reqwest::Response) -> String {
    const MAX_REASON: usize = 512;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let reason = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|json| json.get("error")?.as_str().map(str::to_string))
        .unwrap_or(body);
    if reason.trim().is_empty() {
        return format!("HTTP {status}");
    }
    match reason.char_indices().nth(MAX_REASON) {
        Some((cut, _)) => format!("{}…", &reason[..cut]),
        None => reason,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    fn row(ts: u64, name: &str, labels: &str, value: f64) -> MetricsQueryRow {
        MetricsQueryRow {
            timestamp: ts,
            metric_name: name.to_string(),
            labels: labels.to_string(),
            value,
        }
    }

    #[test]
    fn merge_empty_sources() {
        let result = merge_metrics_results(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn merge_single_source() {
        let source = vec![row(1, "cpu", "{}", 10.0), row(2, "cpu", "{}", 20.0)];
        let result = merge_metrics_results(vec![source]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].timestamp, 1);
    }

    #[test]
    fn merge_multiple_sources_sorted() {
        let s1 = vec![row(1, "cpu", "{}", 10.0), row(3, "cpu", "{}", 30.0)];
        let s2 = vec![row(2, "cpu", "{}", 20.0), row(4, "cpu", "{}", 40.0)];
        let result = merge_metrics_results(vec![s1, s2]);
        assert_eq!(result.len(), 4);
        assert_eq!(result[0].timestamp, 1);
        assert_eq!(result[1].timestamp, 2);
        assert_eq!(result[2].timestamp, 3);
        assert_eq!(result[3].timestamp, 4);
    }

    #[test]
    fn merge_deduplicates_same_timestamp_name_labels() {
        let s1 = vec![row(1, "cpu", "{}", 10.0)];
        let s2 = vec![row(1, "cpu", "{}", 10.0)];
        let result = merge_metrics_results(vec![s1, s2]);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn merge_keeps_different_metrics_at_same_timestamp() {
        let s1 = vec![row(1, "cpu", "{}", 10.0)];
        let s2 = vec![row(1, "mem", "{}", 20.0)];
        let result = merge_metrics_results(vec![s1, s2]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn merge_keeps_different_labels_at_same_timestamp() {
        let s1 = vec![row(1, "cpu", r#"{"app":"web"}"#, 10.0)];
        let s2 = vec![row(1, "cpu", r#"{"app":"api"}"#, 20.0)];
        let result = merge_metrics_results(vec![s1, s2]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn merge_handles_overlapping_ranges() {
        let s1 = vec![
            row(1, "cpu", "{}", 10.0),
            row(2, "cpu", "{}", 20.0),
            row(3, "cpu", "{}", 30.0),
        ];
        let s2 = vec![
            row(2, "cpu", "{}", 20.0),
            row(3, "cpu", "{}", 30.0),
            row(4, "cpu", "{}", 40.0),
        ];
        let result = merge_metrics_results(vec![s1, s2]);
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn equal_rows_from_two_sources_are_merged_even_when_not_adjacent() {
        let result = merge_metrics_results(vec![
            vec![row(10, "cpu", "{}", 1.0), row(10, "mem", "{}", 2.0)],
            vec![row(10, "cpu", "{}", 1.0), row(10, "mem", "{}", 2.0)],
        ]);
        assert_eq!(result.len(), 2, "{result:?}");
    }

    #[test]
    fn a_node_answer_at_the_row_limit_is_reported_as_truncated() {
        use crate::mayo::store::APP_QUERY_ROW_LIMIT;
        let full: Vec<_> = (0..APP_QUERY_ROW_LIMIT as u64)
            .map(|ts| row(ts, "cpu", r#"{"node":"a"}"#, 1.0))
            .collect();
        let short = vec![
            row(1, "cpu", r#"{"node":"b"}"#, 1.0),
            row(2, "cpu", r#"{"node":"b"}"#, 1.0),
            row(3, "cpu", r#"{"node":"b"}"#, 1.0),
        ];
        let result = merge_app_answers(
            vec![
                ("http://node-a:9117".to_string(), full),
                ("http://node-b:9117".to_string(), short),
            ],
            vec![],
        );
        assert_eq!(
            result.warnings,
            vec![QueryWarning::Truncated {
                node_id: "http://node-a:9117".to_string(),
                limit: APP_QUERY_ROW_LIMIT,
            }]
        );
        assert_eq!(result.data.len(), APP_QUERY_ROW_LIMIT + 3);
    }

    #[test]
    fn merge_three_sources() {
        let s1 = vec![row(1, "cpu", "{}", 10.0)];
        let s2 = vec![row(2, "cpu", "{}", 20.0)];
        let s3 = vec![row(3, "cpu", "{}", 30.0)];
        let result = merge_metrics_results(vec![s1, s2, s3]);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].value, 10.0);
        assert_eq!(result[1].value, 20.0);
        assert_eq!(result[2].value, 30.0);
    }

    #[tokio::test]
    async fn cluster_query_counts_reassigned_worker_history_once() {
        use axum::{Json, Router, routing::get};
        let mut servers = Vec::new();
        let mut urls = Vec::new();
        for node in ["worker-b", "worker-c"] {
            let rows = serde_json::json!([
                {"node_id":"worker-a", "timestamp":1000, "metric_name":"cpu", "labels":"{}", "value":10.0},
                {"node_id":node, "timestamp":1000, "metric_name":"cpu", "labels":"{}", "value":20.0}
            ]);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            urls.push(format!("http://{}", listener.local_addr().unwrap()));
            let handler = get(move || {
                let rows = rows.clone();
                async move { Json(rows) }
            });
            let app = Router::new()
                .route("/v1/metrics/rollup", handler.clone())
                .route("/v1/metrics/rollup/owned", handler);
            servers.push(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
        }
        let query = MetricsQuery {
            metric_name: Some("cpu".into()),
            start: 0,
            end: 2000,
            app: None,
            per_series: None,
        };
        let result = fan_out_cluster_query(
            &query,
            &urls,
            &reqwest::Client::new(),
            Duration::from_secs(2),
            None,
        )
        .await;
        for server in servers {
            server.abort();
            let _ = server.await;
        }
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result.data.len(), 1);
        assert_eq!(
            result.data[0].value, 50.0,
            "reassigned worker counted twice"
        );
    }

    #[test]
    fn conflicting_copies_are_unknown_and_other_series_remain_independent() {
        let owned = |value| OwnedRollupRow {
            node_id: "worker-a".into(),
            row: row(1000, "cpu", "{}", value),
        };
        let independent = OwnedRollupRow {
            node_id: "worker-a".into(),
            row: row(1000, "cpu", r#"{"app":"web"}"#, 30.0),
        };
        let result = merge_owned_rollups(vec![
            vec![owned(10.0), independent],
            vec![owned(20.0), owned(10.0)],
        ]);
        assert_eq!(result.data.len(), 1);
        assert_eq!(result.data[0].value, 30.0);
        assert_eq!(
            result.warnings,
            vec![QueryWarning::DataUnavailable {
                node_id: "worker-a".into(),
                from: 1000,
                to: 1000
            }]
        );
    }

    #[tokio::test]
    async fn unowned_or_malformed_rollup_responses_remain_unknown() {
        use axum::{Json, Router, routing::get};
        for owned_endpoint in [false, true] {
            let app = Router::new().route(
                if owned_endpoint {
                    "/v1/metrics/rollup/owned"
                } else {
                    "/v1/metrics/rollup"
                },
                get(|| async { Json(vec![row(1000, "cpu", "{}", 10.0)]) }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let query = MetricsQuery {
                metric_name: None,
                start: 0,
                end: 2000,
                app: None,
                per_series: None,
            };
            let result = fan_out_cluster_query(
                &query,
                &[url],
                &reqwest::Client::new(),
                Duration::from_secs(1),
                None,
            )
            .await;
            server.abort();
            let _ = server.await;
            assert!(result.data.is_empty());
            assert_eq!(result.warnings.len(), 1);
        }
    }

    /// Serve `handler` at the owned-rollup route on a local port.
    async fn serve_owned(
        handler: axum::routing::MethodRouter,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new().route("/v1/metrics/rollup/owned", handler);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, server)
    }

    fn cpu_query() -> MetricsQuery {
        MetricsQuery {
            metric_name: Some("cpu".into()),
            start: 0,
            end: 100_000,
            app: None,
            per_series: None,
        }
    }

    /// #2451: a member that answered "narrow the time range" was reported
    /// as unresponsive, because the error body was thrown away. Its reason
    /// now reaches the caller.
    #[tokio::test]
    async fn a_member_that_refuses_the_query_is_not_reported_as_unresponsive() {
        use axum::http::StatusCode;
        let (url, server) = serve_owned(axum::routing::get(|| async {
            (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "query rejected: narrow the time range"})),
            )
        }))
        .await;
        let result = fan_out_cluster_query(
            &cpu_query(),
            std::slice::from_ref(&url),
            &reqwest::Client::new(),
            Duration::from_secs(2),
            None,
        )
        .await;
        server.abort();
        assert_eq!(
            result.warnings,
            [QueryWarning::QueryRefused {
                node_id: url,
                reason: "query rejected: narrow the time range".into(),
            }]
        );
    }

    /// #2451: a member answers a long range in pages, naming where the next
    /// one starts; the entry node reads them all.
    #[tokio::test]
    async fn a_member_answer_in_pages_is_read_to_the_end() {
        use axum::extract::Query;
        use std::collections::HashMap;
        let (url, server) = serve_owned(axum::routing::get(
            |Query(params): Query<HashMap<String, String>>| async move {
                let start: u64 = params["start"].parse().unwrap();
                let owned = |timestamp: u64| {
                    serde_json::json!({"node_id": "w1", "timestamp": timestamp,
                        "metric_name": "cpu", "labels": "{}", "value": 1.0})
                };
                if start < 120 {
                    ([(NEXT_START_HEADER, "120")], axum::Json(vec![owned(60)])).into_response()
                } else {
                    axum::Json(vec![owned(120), owned(180)]).into_response()
                }
            },
        ))
        .await;
        let result = fan_out_cluster_query(
            &cpu_query(),
            &[url],
            &reqwest::Client::new(),
            Duration::from_secs(2),
            None,
        )
        .await;
        server.abort();
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        let stamps: Vec<u64> = result.data.iter().map(|row| row.timestamp).collect();
        assert_eq!(stamps, [60, 120, 180]);
    }

    // --- merge_cluster_results tests ---

    #[test]
    fn cluster_merge_empty_sources() {
        let result = merge_cluster_results(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn cluster_merge_sums_same_key() {
        // Each council member has a partial sum for the same timestamp/metric
        let c1 = vec![row(1000, "cpu", "{}", 30.0)]; // c1 holds w1+w2
        let c2 = vec![row(1000, "cpu", "{}", 70.0)]; // c2 holds w3+w4
        let c3 = vec![row(1000, "cpu", "{}", 50.0)]; // c3 holds w5
        let result = merge_cluster_results(vec![c1, c2, c3]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].value, 150.0);
    }

    #[test]
    fn cluster_merge_different_timestamps() {
        let c1 = vec![row(1000, "cpu", "{}", 10.0), row(1060, "cpu", "{}", 15.0)];
        let c2 = vec![row(1000, "cpu", "{}", 20.0), row(1060, "cpu", "{}", 25.0)];
        let result = merge_cluster_results(vec![c1, c2]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].value, 30.0); // 10 + 20
        assert_eq!(result[1].value, 40.0); // 15 + 25
    }

    #[test]
    fn cluster_merge_different_metrics() {
        let c1 = vec![row(1000, "cpu", "{}", 10.0), row(1000, "mem", "{}", 512.0)];
        let c2 = vec![row(1000, "cpu", "{}", 20.0), row(1000, "mem", "{}", 1024.0)];
        let result = merge_cluster_results(vec![c1, c2]);
        assert_eq!(result.len(), 2);
        // BTreeMap ordering: "cpu" < "mem"
        assert_eq!(result[0].metric_name, "cpu");
        assert_eq!(result[0].value, 30.0);
        assert_eq!(result[1].metric_name, "mem");
        assert_eq!(result[1].value, 1536.0);
    }

    #[test]
    fn cluster_merge_single_source() {
        let c1 = vec![row(1000, "cpu", "{}", 42.0)];
        let result = merge_cluster_results(vec![c1]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].value, 42.0);
    }
}
