//! Cross-node log query coordination.
//!
//! When an app runs on multiple nodes, the leader fans out the log query
//! to each node's `/v1/logs/entries` API and merges the results in ingest
//! order. Failure modes earlier versions got wrong:
//!
//! - **Silent failures (OBS6).** A node that was unreachable, returned
//!   non-2xx, sent unparseable JSON, or whose task panicked was folded into
//!   an empty success, so "no logs" and "half the cluster is down" looked
//!   identical. Fan-out now reports which nodes failed alongside the entries
//!   it did collect (a partial result).
//! - **Content-based dedup (OBS6, V02).** Merging first deduped adjacent
//!   `(timestamp, line)` pairs, then any `(node, timestamp, stream, line)`.
//!   Both collapse a line an app really did print twice in one second. Each
//!   node now stamps every row with a unique, rising `sequence`, so dedup is
//!   keyed on `(node, sequence)`: the same row reported twice collapses and
//!   nothing else does.
//! - **One-second ordering (V02).** Sorting on the one-second timestamp left
//!   rows within a second in whatever order they arrived. Rows now sort on
//!   `sequence` (nanoseconds, rising strictly per node), so each instance's
//!   lines come back in the order it wrote them.

use std::collections::HashSet;

use super::types::{KetchupError, LogEntry, LogQuery};

/// One node's contribution to a fan-out: its id and the entries it returned.
pub struct NodeLogs {
    /// Stable node identity, used as the dedup key alongside the row's sequence.
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

/// Which nodes a cluster-wide log query asks, and which it can't reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryTargets {
    /// `(node id, API address)` for every node to ask.
    pub reachable: Vec<(String, String)>,
    /// Nodes the query should ask but that have no live membership entry.
    pub unreachable: Vec<String>,
}

/// Pick the nodes a log query fans out to: every live member.
///
/// A node stores the lines of the instances it ran, and keeps them after the
/// app moves on. So an app's history is spread over every node it ever ran
/// on, which placement (where it runs *now*) doesn't record. Asking only the
/// placed nodes returned whatever those nodes last stored as "the tail",
/// however old. A node that never ran the app answers with nothing.
///
/// `placed` is where the scheduler runs the app now; a placed node missing
/// from `members` (every live member as `(node id, API address)`) is
/// reported unreachable, because its lines are certainly wanted.
pub fn query_targets(placed: &[String], members: &[(String, String)]) -> QueryTargets {
    QueryTargets {
        reachable: members.to_vec(),
        unreachable: placed
            .iter()
            .filter(|node| !members.iter().any(|(id, _)| id == *node))
            .cloned()
            .collect(),
    }
}

/// Merge entries from multiple nodes in ingest order, deduplicating by
/// `(node, sequence)`.
///
/// Each node's sequence rises strictly, so a node's own rows keep their
/// order. Rows from different nodes interleave by their nanosecond
/// sequences, with the node id breaking an exact tie so the result is
/// deterministic.
pub fn merge_node_logs(sources: Vec<NodeLogs>) -> Vec<LogEntry> {
    let mut seen: HashSet<(String, u64)> = HashSet::new();
    let mut merged: Vec<(String, LogEntry)> = Vec::new();

    for source in sources {
        for entry in source.entries {
            if seen.insert((source.node_id.clone(), entry.sequence)) {
                merged.push((source.node_id.clone(), entry));
            }
        }
    }

    merged.sort_by(|(left_node, left), (right_node, right)| {
        (left.sequence, left_node).cmp(&(right.sequence, right_node))
    });
    merged.into_iter().map(|(_, entry)| entry).collect()
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
/// splitting into extra query parameters. Each node's deadline includes headers
/// and the complete response body. Dropping this query aborts its owned requests;
/// a timed-out node is reported alongside results from responsive nodes.
pub async fn fan_out_query(
    query: &LogQuery,
    nodes: &[(String, String)],
    client: &reqwest::Client,
    timeout: std::time::Duration,
    service_token: Option<&str>,
) -> Result<FanOutResult, KetchupError> {
    let mut handles = tokio::task::JoinSet::new();

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

        handles.spawn(async move {
            // Build the target URL through `url::Url` so `app`/`namespace`
            // path segments are percent-encoded, and hand the query pairs to
            // reqwest's `.query()`, which encodes each value. A `grep` value
            // with `&` or `?` therefore travels as one parameter's data, not
            // as extra query syntax.
            let outcome: Result<Vec<LogEntry>, String> = tokio::time::timeout(timeout, async {
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

                let response = request
                    .send()
                    .await
                    .map_err(|error| format!("request failed: {error}"))?;
                if !response.status().is_success() {
                    return Err(format!("status {}", response.status().as_u16()));
                }
                response
                    .json::<Vec<LogEntry>>()
                    .await
                    .map_err(|error| format!("invalid json: {error}"))
            })
            .await
            .unwrap_or_else(|_| Err("timed out".to_string()));
            (node_id, outcome)
        });
    }

    let mut sources = Vec::new();
    let mut failures = Vec::new();
    while let Some(result) = handles.join_next().await {
        match result {
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

    fn entry(sequence: u64, line: &str) -> LogEntry {
        LogEntry {
            timestamp: sequence,
            sequence,
            instance: None,
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

    async fn stalled_body_server() -> (
        String,
        tokio::sync::oneshot::Receiver<()>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (headers, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n[").await.unwrap();
            headers.send(()).unwrap();
            // Cancelling a body with unread response bytes may reset TCP rather
            // than send FIN. Both prove closure; other errors remain failures.
            match socket.read(&mut buffer).await {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                other => panic!("query did not close its socket: {other:?}"),
            }
        });
        (url, ready, task)
    }

    #[tokio::test]
    async fn node_deadline_includes_a_stalled_response_body() {
        let (url, _ready, mut server) = stalled_body_server().await;
        let client = reqwest::Client::new();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fan_out_query(
                &log_query(None),
                &[("stalled".into(), url)],
                &client,
                std::time::Duration::from_millis(100),
                None,
            ),
        )
        .await;
        server.abort();
        let _ = (&mut server).await;
        let result = result.expect("headers escaped the query deadline").unwrap();
        assert!(result.entries.is_empty());
        assert_eq!(result.failures.len(), 1);
        assert_eq!(result.failures[0].node_id, "stalled");
        assert!(result.failures[0].reason.contains("timed out"));
    }

    #[tokio::test]
    async fn cancelling_fan_out_releases_inflight_body_reads() {
        let (url, ready, mut server) = stalled_body_server().await;
        let task = tokio::spawn(async move {
            fan_out_query(
                &log_query(None),
                &[("stalled".into(), url)],
                &reqwest::Client::new(),
                std::time::Duration::from_secs(30),
                None,
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        let closed = tokio::time::timeout(std::time::Duration::from_secs(2), &mut server).await;
        server.abort();
        closed
            .expect("cancelled query left a detached request")
            .unwrap();
    }

    fn members(ids: &[&str]) -> Vec<(String, String)> {
        ids.iter()
            .map(|id| (id.to_string(), format!("https://{id}:9117")))
            .collect()
    }

    fn reachable_ids(targets: &QueryTargets) -> Vec<&str> {
        targets
            .reachable
            .iter()
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// V02 soak: soak-redis-client ran on node 1, then node 3, then node 2
    /// for half an hour. After a whole-cluster restart it was placed on node
    /// 1 again, the query asked node 1 alone, and node 1's newest stored lines
    /// were half an hour old. Every live member holds some of an app's
    /// history, so every live member is asked.
    #[test]
    fn a_query_asks_every_live_member_not_just_where_the_app_runs_now() {
        let targets = query_targets(&["n1".to_string()], &members(&["n1", "n2", "n3"]));
        assert_eq!(reachable_ids(&targets), vec!["n1", "n2", "n3"]);
        assert!(targets.unreachable.is_empty());
    }

    /// Right after a restart nothing may be placed yet; the history is still
    /// on disk and the query still answers from it.
    #[test]
    fn an_unplaced_app_is_still_queried_everywhere() {
        let targets = query_targets(&[], &members(&["n1", "n2"]));
        assert_eq!(reachable_ids(&targets), vec!["n1", "n2"]);
    }

    /// A placed node gossip no longer lists can't be asked, and the answer
    /// says so rather than looking complete.
    #[test]
    fn a_placed_node_missing_from_membership_is_reported_unreachable() {
        let targets = query_targets(&["n4".to_string()], &members(&["n1"]));
        assert_eq!(reachable_ids(&targets), vec!["n1"]);
        assert_eq!(targets.unreachable, vec!["n4".to_string()]);
    }

    #[test]
    fn merge_empty_sources() {
        assert!(merge_node_logs(vec![]).is_empty());
    }

    #[test]
    fn merge_single_source_sorted() {
        let result = merge_node_logs(vec![node("n1", vec![entry(2, "b"), entry(1, "a")])]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].sequence, 1);
        assert_eq!(result[1].sequence, 2);
    }

    #[test]
    fn merge_multiple_sources_sorted() {
        let result = merge_node_logs(vec![
            node("n1", vec![entry(1, "a"), entry(3, "c")]),
            node("n2", vec![entry(2, "b"), entry(4, "d")]),
        ]);
        assert_eq!(result.len(), 4);
        let sequences: Vec<u64> = result.iter().map(|e| e.sequence).collect();
        assert_eq!(sequences, vec![1, 2, 3, 4]);
    }

    /// V02 soak regression: ten lines a second share one `timestamp`. The
    /// merge must keep them in the order the node sequenced them, not in
    /// whatever order they arrived.
    #[test]
    fn lines_sharing_a_second_come_back_in_sequence_order() {
        let mut shuffled: Vec<LogEntry> = (0..10)
            .map(|i| LogEntry {
                timestamp: 1_790_368_624,
                sequence: 1_790_368_624_000_000_000 + i * 100_000_000,
                instance: Some("soak-writer-0".to_string()),
                stream: LogStream::Stdout,
                line: format!("ACK {}", 968 + i),
            })
            .collect();
        shuffled.swap(1, 6);
        shuffled.swap(3, 7);

        let lines: Vec<String> = merge_node_logs(vec![node("n2", shuffled)])
            .into_iter()
            .map(|entry| entry.line)
            .collect();
        let expected: Vec<String> = (968..978).map(|n| format!("ACK {n}")).collect();
        assert_eq!(lines, expected);
    }

    /// An app that prints the same line twice in one second printed two
    /// lines; content-based dedup used to collapse them into one.
    #[test]
    fn repeated_identical_lines_from_one_node_both_survive() {
        let result = merge_node_logs(vec![node(
            "n1",
            vec![entry(1, "dup"), entry(2, "other"), entry(3, "dup")],
        )]);
        let lines: Vec<&str> = result.iter().map(|e| e.line.as_str()).collect();
        assert_eq!(lines, vec!["dup", "other", "dup"]);
    }

    /// The M4 case: two replicas each report the SAME line at the SAME
    /// instant. These are DISTINCT events (one per replica) and both must
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

    /// Two nodes whose sequences tie exactly come back in node order, so the
    /// same query always renders the same way.
    #[test]
    fn exact_sequence_ties_break_on_node_id() {
        let result = merge_node_logs(vec![
            node("n2", vec![entry(5, "from n2")]),
            node("n1", vec![entry(5, "from n1")]),
        ]);
        let lines: Vec<&str> = result.iter().map(|e| e.line.as_str()).collect();
        assert_eq!(lines, vec!["from n1", "from n2"]);
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
