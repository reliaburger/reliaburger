//! Cross-node log query coordination.
//!
//! When an app runs on multiple nodes, the leader fans out the log query
//! to each node's `/v1/logs/entries` API and merges the results by
//! timestamp. Two failure modes the first version got wrong (OBS6):
//!
//! - **Silent failures.** A node that was unreachable, returned non-2xx,
//!   sent unparseable JSON, or whose task panicked was folded into an empty
//!   success, so "no logs" and "half the cluster is down" looked identical.
//!   Fan-out now reports which nodes failed alongside the entries it did
//!   collect (a partial result).
//! - **Adjacency-only dedup.** Merging deduped only *adjacent* equal
//!   `(timestamp, line)` pairs after the sort, which both dropped distinct
//!   events that happened to share a timestamp and kept duplicates that a
//!   third line sorted between. Dedup is now keyed on a stable identity —
//!   `(node, timestamp, stream, line)` — so the same event reported twice
//!   collapses while genuinely distinct events from two replicas survive.

use std::collections::HashSet;

use super::types::{KetchupError, LogEntry, LogQuery};

/// One node's contribution to a fan-out: its id and the entries it returned.
pub struct NodeLogs {
    /// Stable node identity, used as the dedup key alongside the event fields.
    pub node_id: String,
    /// Entries this node returned.
    pub entries: Vec<LogEntry>,
}

/// A node that failed to answer a fan-out query, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeFailure {
    /// Which node failed.
    pub node_id: String,
    /// A short reason (unreachable, non-2xx status, bad JSON, task error).
    pub reason: String,
}

/// The outcome of a fan-out: merged entries plus any per-node failures.
///
/// A partial result — `entries` holds what the reachable nodes returned and
/// `failures` names the ones that didn't, so the caller can tell "no logs"
/// apart from "some replicas were down".
pub struct FanOutResult {
    /// Merged, sorted, deduplicated entries from the nodes that answered.
    pub entries: Vec<LogEntry>,
    /// Nodes that failed to answer.
    pub failures: Vec<NodeFailure>,
}

/// Merge and sort entries from multiple nodes by timestamp, deduplicating by
/// stable `(node, timestamp, stream, line)` identity.
///
/// The same event reported twice by one node collapses to one row. Two
/// replicas that each logged an identical line at the same instant are
/// *distinct* events (different `node_id`) and both survive.
pub fn merge_node_logs(sources: Vec<NodeLogs>) -> Vec<LogEntry> {
    let mut seen: HashSet<(String, u64, &'static str, String)> = HashSet::new();
    let mut merged: Vec<LogEntry> = Vec::new();

    for source in sources {
        for entry in source.entries {
            let stream_tag = match entry.stream {
                super::types::LogStream::Stdout => "O",
                super::types::LogStream::Stderr => "E",
            };
            let key = (
                source.node_id.clone(),
                entry.timestamp,
                stream_tag,
                entry.line.clone(),
            );
            if seen.insert(key) {
                merged.push(entry);
            }
        }
    }

    merged.sort_by_key(|e| e.timestamp);
    merged
}

/// Build `{base}/v1/logs/entries/{app}/{namespace}` with the app and
/// namespace percent-encoded as path segments.
fn build_entries_url(base: &str, app: &str, namespace: &str) -> Result<url::Url, url::ParseError> {
    let mut url = url::Url::parse(base)?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| url::ParseError::RelativeUrlWithoutBase)?;
        // Keep any existing base path, then append our fixed segments. `push`
        // percent-encodes each segment, so a slash or space in `app` stays
        // within one segment.
        segments.extend(["v1", "logs", "entries", app, namespace]);
    }
    Ok(url)
}

/// Fan out a log query to multiple nodes, collect results, merge.
///
/// `nodes` pairs each node's stable id with its base URL (e.g.
/// `http://10.0.1.5:9117`). The query is sent to
/// `GET /v1/logs/entries/{app}/{namespace}` with URL-encoded parameters, so
/// a `grep` value containing `&` or `?` is transmitted intact rather than
/// splitting into extra query parameters.
pub async fn fan_out_query(
    query: &LogQuery,
    nodes: &[(String, String)],
    client: &reqwest::Client,
    timeout: std::time::Duration,
    service_token: Option<&str>,
) -> Result<FanOutResult, KetchupError> {
    let mut handles = Vec::new();

    for (node_id, url) in nodes {
        let node_id = node_id.clone();
        let base = url.clone();
        let app = query.app.clone();
        let namespace = query.namespace.clone();
        let grep = query.grep.clone();
        let token = service_token.map(str::to_string);
        let tail = query.tail;
        let start = query.start;
        let end = query.end;
        let client = client.clone();

        handles.push(tokio::spawn(async move {
            // Build the target URL through `url::Url` so `app`/`namespace`
            // path segments are percent-encoded, and hand the query pairs to
            // reqwest's `.query()`, which encodes each value. A `grep` value
            // with `&` or `?` therefore travels as one parameter's data, not
            // as extra query syntax.
            let outcome: Result<Vec<LogEntry>, String> = async {
                let req_url = build_entries_url(&base, &app, &namespace)
                    .map_err(|e| format!("bad url: {e}"))?;

                let mut params: Vec<(&str, String)> = Vec::new();
                if let Some(t) = tail {
                    params.push(("tail", t.to_string()));
                }
                if let Some(ref g) = grep {
                    params.push(("grep", g.clone()));
                }
                if let Some(s) = start {
                    params.push(("start", s.to_string()));
                }
                if let Some(e) = end {
                    params.push(("end", e.to_string()));
                }

                let request =
                    crate::sesame::auth::bearer_get(&client, req_url.as_str(), token.as_deref())
                        .query(&params);

                match tokio::time::timeout(timeout, request.send()).await {
                    Ok(Ok(r)) if r.status().is_success() => r
                        .json::<Vec<LogEntry>>()
                        .await
                        .map_err(|e| format!("invalid json: {e}")),
                    Ok(Ok(r)) => Err(format!("status {}", r.status().as_u16())),
                    Ok(Err(e)) => Err(format!("request failed: {e}")),
                    Err(_) => Err("timed out".to_string()),
                }
            }
            .await;
            (node_id, outcome)
        }));
    }

    let mut sources = Vec::new();
    let mut failures = Vec::new();
    for handle in handles {
        match handle.await {
            Ok((node_id, Ok(entries))) => sources.push(NodeLogs { node_id, entries }),
            Ok((node_id, Err(reason))) => failures.push(NodeFailure { node_id, reason }),
            Err(e) => failures.push(NodeFailure {
                node_id: "unknown".to_string(),
                reason: format!("task error: {e}"),
            }),
        }
    }

    Ok(FanOutResult {
        entries: merge_node_logs(sources),
        failures,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ketchup::types::LogStream;

    fn entry(ts: u64, line: &str) -> LogEntry {
        LogEntry {
            timestamp: ts,
            stream: LogStream::Stdout,
            line: line.to_string(),
        }
    }

    fn node(id: &str, entries: Vec<LogEntry>) -> NodeLogs {
        NodeLogs {
            node_id: id.to_string(),
            entries,
        }
    }

    #[test]
    fn merge_empty_sources() {
        assert!(merge_node_logs(vec![]).is_empty());
    }

    #[test]
    fn merge_single_source_sorted() {
        let result = merge_node_logs(vec![node("n1", vec![entry(2, "b"), entry(1, "a")])]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].timestamp, 1);
        assert_eq!(result[1].timestamp, 2);
    }

    #[test]
    fn merge_multiple_sources_sorted() {
        let result = merge_node_logs(vec![
            node("n1", vec![entry(1, "a"), entry(3, "c")]),
            node("n2", vec![entry(2, "b"), entry(4, "d")]),
        ]);
        assert_eq!(result.len(), 4);
        let timestamps: Vec<u64> = result.iter().map(|e| e.timestamp).collect();
        assert_eq!(timestamps, vec![1, 2, 3, 4]);
    }

    /// The same event reported twice by ONE node collapses to a single row —
    /// even when a different line at the same timestamp sorts between the two
    /// duplicates (the case adjacency-only dedup missed).
    #[test]
    fn separated_duplicates_from_one_node_dedup() {
        let result = merge_node_logs(vec![node(
            "n1",
            vec![entry(1, "dup"), entry(1, "other"), entry(1, "dup")],
        )]);
        // "dup" appears once; "other" survives.
        let dups = result.iter().filter(|e| e.line == "dup").count();
        assert_eq!(dups, 1, "duplicate event from one node not collapsed");
        assert_eq!(result.len(), 2);
    }

    /// The M4 case: two replicas each report the SAME line at the SAME
    /// timestamp. These are DISTINCT events (one per replica) and both must
    /// survive, because the dedup key includes the node identity.
    #[test]
    fn identical_lines_from_two_replicas_both_survive() {
        let result = merge_node_logs(vec![
            node("n1", vec![entry(5, "request handled")]),
            node("n2", vec![entry(5, "request handled")]),
        ]);
        assert_eq!(
            result.len(),
            2,
            "distinct per-replica events were merged away"
        );
    }

    /// A node whose response is duplicated across the wire (retransmit) still
    /// dedups within that node.
    #[test]
    fn cross_source_duplicates_from_same_node_dedup() {
        let result = merge_node_logs(vec![
            node("n1", vec![entry(1, "x")]),
            node("n1", vec![entry(1, "x")]),
        ]);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn stdout_and_stderr_at_same_timestamp_are_distinct() {
        let out = entry(1, "same");
        let err = LogEntry {
            timestamp: 1,
            stream: LogStream::Stderr,
            line: "same".to_string(),
        };
        let result = merge_node_logs(vec![node("n1", vec![out, err])]);
        assert_eq!(result.len(), 2, "different streams must not dedup together");
    }

    #[test]
    fn entries_url_encodes_path_segments() {
        let url = build_entries_url("http://10.0.1.5:9117", "my app", "team/ns").unwrap();
        // Space and slash inside a segment must be percent-encoded, not
        // treated as a path separator.
        assert_eq!(
            url.as_str(),
            "http://10.0.1.5:9117/v1/logs/entries/my%20app/team%2Fns"
        );
    }

    // -- fan-out over a real local HTTP server (deterministic) --------------

    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    fn log_query(grep: Option<&str>) -> LogQuery {
        LogQuery {
            app: "web".to_string(),
            namespace: "default".to_string(),
            grep: grep.map(str::to_string),
            ..Default::default()
        }
    }

    /// A `grep` value containing `&` and `?` must reach the node as one
    /// parameter's data, not split into extra query parameters (OBS6).
    #[tokio::test]
    async fn grep_value_with_ampersand_and_question_mark_transmitted_intact() {
        use axum::Router;
        use axum::extract::{Path, Query};
        use axum::routing::get;
        use std::collections::HashMap;

        let received: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let received_clone = Arc::clone(&received);
        let app = Router::new().route(
            "/v1/logs/entries/{app}/{namespace}",
            get(
                move |Path((_a, _n)): Path<(String, String)>,
                      Query(params): Query<HashMap<String, String>>| {
                    let received = Arc::clone(&received_clone);
                    async move {
                        *received.lock().await = params.get("grep").cloned();
                        axum::Json(Vec::<LogEntry>::new())
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let nodes = vec![("n1".to_string(), format!("http://{addr}"))];

        let tricky = "err&code?x=1";
        let result = fan_out_query(
            &log_query(Some(tricky)),
            &nodes,
            &client,
            Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
        assert!(
            result.failures.is_empty(),
            "unexpected failures: {:?}",
            result.failures
        );

        // Poll for the captured value rather than sleeping.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let got = loop {
            if let Some(g) = received.lock().await.clone() {
                break g;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("node never received the request");
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(got, tricky, "grep value mangled in transit");
    }

    /// An unreachable node is reported as a partial failure, not folded into
    /// a silent empty success (OBS6).
    #[tokio::test]
    async fn unreachable_node_is_a_partial_failure() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        // Port 1 on loopback refuses connections immediately.
        let nodes = vec![("dead-node".to_string(), "http://127.0.0.1:1".to_string())];

        let result = fan_out_query(
            &log_query(None),
            &nodes,
            &client,
            Duration::from_millis(500),
            None,
        )
        .await
        .unwrap();

        assert!(result.entries.is_empty());
        assert_eq!(
            result.failures.len(),
            1,
            "failure was swallowed as empty success"
        );
        assert_eq!(result.failures[0].node_id, "dead-node");
    }

    /// A node returning a non-2xx status is a failure, not empty success.
    #[tokio::test]
    async fn error_status_node_is_a_partial_failure() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::get;

        let app = Router::new().route(
            "/v1/logs/entries/{app}/{namespace}",
            get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let nodes = vec![("n1".to_string(), format!("http://{addr}"))];

        let result = fan_out_query(
            &log_query(None),
            &nodes,
            &client,
            Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.failures.len(), 1);
        assert!(result.failures[0].reason.contains("status 500"));
    }
}
