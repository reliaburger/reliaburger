//! Log routes: queries, follow streams (SSE and WebSocket), cross-node
//! reads, SQL and export.

use super::*;

/// Query parameters for the logs endpoint.
#[derive(Deserialize)]
pub(super) struct LogsQuery {
    pub(super) tail: Option<usize>,
    pub(super) follow: Option<bool>,
    pub(super) start: Option<u64>,
    pub(super) end: Option<u64>,
    pub(super) grep: Option<String>,
    /// Only this instance's lines (`default__web-0`).
    pub(super) instance: Option<String>,
    /// Only lines written to this stream: `stdout` or `stderr`.
    pub(super) stream: Option<String>,
    /// Follow only this node's instances. Set on the internal per-node
    /// streams of a cluster-wide follow, so a peer never fans out again.
    pub(super) local: Option<bool>,
    /// Prefix each followed line with `[node instance]`.
    pub(super) label: Option<bool>,
}

/// Get logs for an app.
///
/// Supports `?tail=N` to return only the last N lines, and
/// `?follow=true` to stream new lines as an SSE stream.
pub(super) async fn logs_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(query): Query<LogsQuery>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Logs,
        &app,
        &namespace,
    )
    .await
    {
        return resp;
    }
    let follow = query.follow.unwrap_or(false);
    // A followed stream's tail comes from the raw capture, which doesn't
    // keep each line's stream; refuse rather than half-filter.
    if follow && query.stream.is_some() {
        return log_query_error(crate::ketchup::types::KetchupError::QueryRejected {
            reason: "stream can't be combined with follow".to_string(),
        });
    }

    if follow {
        // A cluster member follows every node that runs the app; the
        // per-node streams it opens come back here with `local=true`.
        if !query.local.unwrap_or(false)
            && let Some(frames) = spawn_cluster_log_follow(
                &state,
                &app,
                &namespace,
                query.tail,
                query.instance.clone(),
            )
        {
            let stream = ReceiverStream::new(frames)
                .map(|frame| Ok::<_, std::convert::Infallible>(log_frame_event(frame)));
            return Sse::new(stream)
                .keep_alive(axum::response::sse::KeepAlive::default())
                .into_response();
        }
        let label = query
            .label
            .unwrap_or(false)
            .then(|| state.node_name.clone())
            .flatten();
        let lines_rx = match follow_local_logs(
            &state,
            app,
            namespace,
            query.tail,
            query.instance.clone(),
            label,
        )
        .await
        {
            Ok(lines_rx) => lines_rx,
            Err(response) => return response,
        };
        let stream = ReceiverStream::new(lines_rx)
            .map(|line| Ok::<_, std::convert::Infallible>(Event::default().data(line)));
        return Sse::new(stream).into_response();
    }

    match ask_agent(&state.cmd_tx, |response| AgentCommand::Logs {
        app_name: app,
        namespace,
        tail: query.tail,
        response,
    })
    .await
    {
        Ok(Ok(logs)) => Json(serde_json::json!({ "logs": logs })).into_response(),
        Ok(Err(e)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(response) => response,
    }
}

/// Start following this node's instances of an app.
// `Response` is large but it IS the HTTP reply to send on failure;
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
pub(super) async fn follow_local_logs(
    state: &ApiState,
    app: String,
    namespace: String,
    tail: Option<usize>,
    instance: Option<String>,
    label: Option<String>,
) -> Result<mpsc::Receiver<String>, Response> {
    let (lines_tx, lines_rx) = mpsc::channel::<String>(64);
    state
        .cmd_tx
        .send(AgentCommand::FollowLogs {
            app_name: app,
            namespace,
            tail,
            instance,
            label,
            lines: lines_tx,
        })
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "agent unavailable" })),
            )
                .into_response()
        })?;
    Ok(lines_rx)
}

/// Start following an app on every node that runs it, when this node is a
/// cluster member; `None` on a standalone node, which follows itself.
///
/// The SSE and WebSocket endpoints both read the returned frames, so a
/// browser, `relish logs -f` and the TUI see the same merged stream.
pub(super) fn spawn_cluster_log_follow(
    state: &ApiState,
    app: &str,
    namespace: &str,
    tail: Option<usize>,
    instance: Option<String>,
) -> Option<mpsc::Receiver<LogFrame>> {
    let (Some(council), Some(membership), Some(self_name)) =
        (&state.council, &state.membership, &state.node_name)
    else {
        return None;
    };
    let (frames_tx, frames_rx) = mpsc::channel::<LogFrame>(256);
    tokio::spawn(follow_cluster_logs(
        state.clone(),
        Arc::clone(council),
        Arc::clone(membership),
        self_name.clone(),
        app.to_string(),
        namespace.to_string(),
        tail,
        instance,
        frames_tx,
    ));
    Some(frames_rx)
}

/// One followed frame as an SSE event: a warning carries `event: warning`.
pub(super) fn log_frame_event(frame: LogFrame) -> Event {
    match frame {
        LogFrame::Line(line) => Event::default().data(line),
        LogFrame::Warning(warning) => Event::default()
            .event(crate::ketchup::sse::WARNING_EVENT)
            .data(warning),
    }
}

/// How often a cluster-wide follow re-reads placements, to pick up replicas
/// scheduled onto new nodes and to notice nodes that left.
pub(super) const LOG_FOLLOW_REFRESH: std::time::Duration = std::time::Duration::from_secs(2);

/// Why one node's part of a cluster-wide follow stopped.
pub(super) struct LogSourceEnded {
    pub(super) node: String,
    /// `None` when the stream ended cleanly, say because its replica
    /// restarted; the next refresh reconnects without a warning.
    pub(super) error: Option<String>,
}

/// Merge the log streams of every node that runs an app into `events`.
///
/// Every [`LOG_FOLLOW_REFRESH`] it re-reads the app's placements and the live
/// membership: it opens a stream to each placed node it isn't following yet
/// and drops the streams of nodes that left. A node that goes away produces a
/// [`LogFrame::Warning`] and the follow carries on with the rest. It returns
/// when the client disconnects.
#[allow(clippy::too_many_arguments)]
pub(super) async fn follow_cluster_logs(
    state: ApiState,
    council: Arc<crate::council::CouncilNode>,
    membership: Arc<RwLock<Vec<NodeMembershipInfo>>>,
    self_name: String,
    app: String,
    namespace: String,
    tail: Option<usize>,
    instance: Option<String>,
    events: mpsc::Sender<LogFrame>,
) {
    let app_id = crate::meat::types::AppId::new(&app, &namespace);
    let mut sources: std::collections::HashMap<String, tokio::task::AbortHandle> =
        std::collections::HashMap::new();
    let mut connected_before: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut departed: std::collections::HashSet<String> = std::collections::HashSet::new();
    // When each node's last stream ended, so a node with nothing to stream
    // yet is retried once per refresh rather than in a tight loop.
    let mut ended_at: std::collections::HashMap<String, tokio::time::Instant> =
        std::collections::HashMap::new();
    let (ended_tx, mut ended_rx) = mpsc::channel::<LogSourceEnded>(16);
    loop {
        let placed: std::collections::BTreeSet<crate::meat::NodeId> = council
            .desired_state()
            .await
            .scheduling
            .get(&app_id)
            .map(|placements| placements.iter().map(|p| p.node_id.clone()).collect())
            .unwrap_or_default();
        let members = membership.read().await.clone();

        // A node we followed that dropped out of the live membership gets
        // one warning, whether its stream broke, ended cleanly (a graceful
        // shutdown) or is still hanging on a dead connection.
        let alive = |node: &str| members.iter().any(|member| member.node_id.0 == node);
        departed.retain(|node| !alive(node));
        let newly_departed: Vec<String> = connected_before
            .iter()
            .filter(|node| **node != self_name && !alive(node) && !departed.contains(*node))
            .cloned()
            .collect();
        for node in newly_departed {
            if let Some(source) = sources.remove(&node) {
                source.abort();
            }
            let warning = format!("node {node} left the cluster; no longer following its logs");
            if !send_log_warning(&events, warning).await {
                return;
            }
            departed.insert(node);
        }

        for node in placed {
            let cooling = ended_at
                .get(&node.0)
                .is_some_and(|at| at.elapsed() < LOG_FOLLOW_REFRESH);
            if sources.contains_key(&node.0) || cooling {
                continue;
            }
            // Only the first connection replays the tail; a reconnect after
            // a replica restart carries on from new lines.
            let tail = if connected_before.insert(node.0.clone()) {
                tail
            } else {
                None
            };
            let source = if node.0 == self_name {
                spawn_local_log_source(
                    &state,
                    &app,
                    &namespace,
                    tail,
                    instance.clone(),
                    &self_name,
                    events.clone(),
                    ended_tx.clone(),
                )
                .await
            } else {
                let Some(member) = members.iter().find(|member| member.node_id == node) else {
                    continue;
                };
                let url = state.cluster_http.url(
                    &member.address.to_string(),
                    &format!("/v1/logs/{app}/{namespace}"),
                );
                Some(spawn_peer_log_source(
                    &state,
                    node.0.clone(),
                    url,
                    tail,
                    instance.as_deref(),
                    events.clone(),
                    ended_tx.clone(),
                ))
            };
            if let Some(source) = source {
                sources.insert(node.0, source);
            }
        }

        tokio::select! {
            () = events.closed() => break,
            Some(ended) = ended_rx.recv() => {
                sources.remove(&ended.node);
                ended_at.insert(ended.node.clone(), tokio::time::Instant::now());
                if let Some(error) = ended.error
                    && !send_log_warning(&events, format!("node {}: {error}", ended.node)).await
                {
                    break;
                }
            }
            () = tokio::time::sleep(LOG_FOLLOW_REFRESH) => {}
        }
    }
    for source in sources.into_values() {
        source.abort();
    }
}

pub(super) async fn send_log_warning(events: &mpsc::Sender<LogFrame>, warning: String) -> bool {
    events.send(LogFrame::Warning(warning)).await.is_ok()
}

/// Follow this node's own instances as one source of a cluster-wide follow.
#[allow(clippy::too_many_arguments)]
pub(super) async fn spawn_local_log_source(
    state: &ApiState,
    app: &str,
    namespace: &str,
    tail: Option<usize>,
    instance: Option<String>,
    self_name: &str,
    events: mpsc::Sender<LogFrame>,
    ended: mpsc::Sender<LogSourceEnded>,
) -> Option<tokio::task::AbortHandle> {
    let mut lines = follow_local_logs(
        state,
        app.to_string(),
        namespace.to_string(),
        tail,
        instance,
        Some(self_name.to_string()),
    )
    .await
    .ok()?;
    let node = self_name.to_string();
    Some(
        tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                if events.send(LogFrame::Line(line)).await.is_err() {
                    return;
                }
            }
            let _ = ended.send(LogSourceEnded { node, error: None }).await;
        })
        .abort_handle(),
    )
}

/// Stream one peer's labelled log lines into `events`, and report how the
/// stream ended.
pub(super) fn spawn_peer_log_source(
    state: &ApiState,
    node: String,
    url: String,
    tail: Option<usize>,
    instance: Option<&str>,
    events: mpsc::Sender<LogFrame>,
    ended: mpsc::Sender<LogSourceEnded>,
) -> tokio::task::AbortHandle {
    let mut request = state.cluster_http.client().get(url).query(&[
        ("follow", "true"),
        ("local", "true"),
        ("label", "true"),
    ]);
    if let Some(tail) = tail {
        request = request.query(&[("tail", tail)]);
    }
    if let Some(instance) = instance {
        request = request.query(&[("instance", instance)]);
    }
    if let Some(token) = &state.service_token {
        request = request.bearer_auth(token);
    }
    tokio::spawn(async move {
        let error = relay_peer_log_stream(request, &events).await.err();
        let _ = ended.send(LogSourceEnded { node, error }).await;
    })
    .abort_handle()
}

pub(super) async fn relay_peer_log_stream(
    request: reqwest::RequestBuilder,
    events: &mpsc::Sender<LogFrame>,
) -> Result<(), String> {
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), request.send())
        .await
        .map_err(|_| "log stream did not start within 5s".to_string())?
        .map_err(|error| format!("log stream failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("log stream refused: {}", response.status()));
    }
    let mut decoder = crate::ketchup::sse::SseDecoder::default();
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| format!("log stream broke: {error}"))?;
        for event in decoder.push(&chunk) {
            let forwarded = match event.event.as_deref() {
                Some(crate::ketchup::sse::WARNING_EVENT) => LogFrame::Warning(event.data),
                _ => LogFrame::Line(event.data),
            };
            if events.send(forwarded).await.is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Upgrade an authenticated request to a live log stream.
///
/// A cluster member follows every node that runs the app, exactly as the SSE
/// follow does; each text frame is one [`LogFrame`] as JSON.
pub(super) async fn ws_logs_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(query): Query<LogsQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    // Scope is checked *before* the upgrade: once the socket is live there
    // is no response left to refuse with.
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Logs,
        &app,
        &namespace,
    )
    .await
    {
        return resp;
    }
    let frames = match spawn_cluster_log_follow(
        &state,
        &app,
        &namespace,
        query.tail,
        query.instance.clone(),
    ) {
        Some(frames) => frames,
        None => match follow_local_logs(
            &state,
            app,
            namespace,
            query.tail,
            query.instance.clone(),
            None,
        )
        .await
        {
            Ok(lines) => frame_local_lines(lines),
            Err(response) => return response,
        },
    };
    upgrade
        .on_upgrade(move |socket| ws_logs_session(socket, frames))
        .into_response()
}

/// Wrap a standalone node's own followed lines as [`LogFrame::Line`]s.
pub(super) fn frame_local_lines(mut lines: mpsc::Receiver<String>) -> mpsc::Receiver<LogFrame> {
    let (frames_tx, frames_rx) = mpsc::channel(64);
    tokio::spawn(async move {
        while let Some(line) = lines.recv().await {
            if frames_tx.send(LogFrame::Line(line)).await.is_err() {
                return;
            }
        }
    });
    frames_rx
}

pub(super) async fn ws_logs_session(mut socket: WebSocket, mut frames: mpsc::Receiver<LogFrame>) {
    loop {
        tokio::select! {
            frame = frames.recv() => {
                let Some(frame) = frame else { return };
                let Ok(json) = serde_json::to_string(&frame) else { continue };
                if socket.send(Message::Text(json.into())).await.is_err() {
                    return;
                }
            }
            message = socket.recv() => if message.is_none() { return; },
        }
    }
}

/// `GET /v1/logs/entries/{app}/{namespace}?start=S&end=E&grep=G&tail=N`
///
/// Internal structured log query endpoint. Returns `Vec<LogEntry>` as
/// JSON. Called by `fan_out_query` on each node during cross-node queries.
pub(super) async fn logs_entries_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(query): Query<LogsQuery>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Logs,
        &app,
        &namespace,
    )
    .await
    {
        return resp;
    }
    let Some(log_store) = &state.log_store else {
        return Json(Vec::<LogEntry>::new()).into_response();
    };

    let store = log_store.read().await;
    let filter = match log_filter(&query) {
        Ok(filter) => filter,
        Err(e) => return log_query_error(e),
    };
    match store.query_with(&app, &namespace, &filter).await {
        Ok(entries) => Json(entries).into_response(),
        Err(e) => log_query_error(e),
    }
}

/// The store filter a request's query parameters ask for, or why they don't
/// make one.
fn log_filter(
    query: &LogsQuery,
) -> Result<crate::ketchup::log_store::LogFilter, crate::ketchup::types::KetchupError> {
    Ok(crate::ketchup::log_store::LogFilter {
        start: query.start,
        end: query.end,
        grep: query.grep.clone(),
        instance: query.instance.clone(),
        stream: parse_stream(query.stream.as_deref())?,
        tail: query.tail,
    })
}

/// `stdout`, `stderr` or nothing; any other value is refused.
fn parse_stream(
    stream: Option<&str>,
) -> Result<Option<crate::ketchup::types::LogStream>, crate::ketchup::types::KetchupError> {
    stream
        .map(|name| {
            crate::ketchup::types::LogStream::parse(name).ok_or_else(|| {
                crate::ketchup::types::KetchupError::QueryRejected {
                    reason: format!("stream {name:?} is neither stdout nor stderr"),
                }
            })
        })
        .transpose()
}

/// A refused query (a grep pattern that won't compile) is the caller's
/// mistake, so it's a 400 with the reason; anything else is ours.
fn log_query_error(error: crate::ketchup::types::KetchupError) -> Response {
    let status = match error {
        crate::ketchup::types::KetchupError::QueryRejected { .. } => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(serde_json::json!({"error": error.to_string()})),
    )
        .into_response()
}

/// `GET /v1/logs/query/{app}/{namespace}?start=S&end=E&grep=G&tail=N`
///
/// Cross-node log query. Fans out to every live member (an app's lines stay
/// on each node it ever ran on, see [`crate::ketchup::query::query_targets`])
/// and merges the answers in ingest order.
pub(super) async fn logs_cross_node_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(query): Query<LogsQuery>,
) -> Response {
    use crate::meat::types::AppId;

    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Logs,
        &app,
        &namespace,
    )
    .await
    {
        return resp;
    }

    // A bad pattern would fail on every node and come back as an empty
    // result full of warnings; refuse it once, here.
    if let Some(grep) = &query.grep
        && let Err(error) = crate::ketchup::log_store::validate_grep(grep)
    {
        return log_query_error(error);
    }
    let stream = match parse_stream(query.stream.as_deref()) {
        Ok(stream) => stream,
        Err(error) => return log_query_error(error),
    };

    // Build a LogQuery from request params
    let log_query = LogQuery {
        app: app.clone(),
        namespace: namespace.clone(),
        start: query.start,
        end: query.end,
        grep: query.grep.clone(),
        instance: query.instance.clone(),
        stream,
        json_field: None,
        // The newest N cluster-wide are among each node's newest N, so every
        // node sends only its own tail; the merge below trims to N again.
        tail: query.tail,
    };

    // If we have council + membership, do cross-node fan-out
    if let (Some(council), Some(membership)) = (&state.council, &state.membership) {
        let desired = council.desired_state().await;
        let app_id = AppId::new(&app, &namespace);

        // Where the app runs now; its history may be on any live member.
        let placed: Vec<String> = desired
            .scheduling
            .get(&app_id)
            .map(|placements| placements.iter().map(|p| p.node_id.0.clone()).collect())
            .unwrap_or_default();
        let live: Vec<(String, String)> = membership
            .read()
            .await
            .iter()
            .map(|member| {
                (
                    member.node_id.0.clone(),
                    state.cluster_http.url(&member.address.to_string(), ""),
                )
            })
            .collect();
        let targets = crate::ketchup::query::query_targets(&placed, &live);
        let nodes = targets.reachable;
        // A placed node with no membership entry can't be reached at all.
        let mut warnings: Vec<LogQueryWarning> = targets
            .unreachable
            .into_iter()
            .map(LogQueryWarning::from)
            .collect();

        let node_count = nodes.len() + warnings.len();

        // Fan out to all reachable nodes
        let timeout = std::time::Duration::from_secs(10);
        match fan_out_query(
            &log_query,
            &nodes,
            state.cluster_http.client(),
            timeout,
            state.service_token.as_deref(),
        )
        .await
        {
            Ok(result) => {
                let mut entries = result.entries;
                // Each node that failed the fan-out becomes a warning that
                // keeps its cause, so the caller sees "wolf4 timed out after
                // 10s", not a silent empty or a bare "did not respond" (#282).
                warnings.extend(result.failures.into_iter().map(LogQueryWarning::from));
                // Apply tail after merge (fan_out already merge-sorted)
                if let Some(tail) = query.tail
                    && entries.len() > tail
                {
                    entries = entries.split_off(entries.len() - tail);
                }
                Json(LogQueryResult {
                    entries,
                    node_count,
                    warnings,
                })
                .into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        }
    } else {
        // Single-node mode: query local log store
        let Some(log_store) = &state.log_store else {
            return Json(LogQueryResult {
                entries: vec![],
                node_count: 1,
                warnings: vec![],
            })
            .into_response();
        };

        let filter = match log_filter(&query) {
            Ok(filter) => filter,
            Err(e) => return log_query_error(e),
        };
        let store = log_store.read().await;
        match store.query_with(&app, &namespace, &filter).await {
            Ok(entries) => Json(LogQueryResult {
                entries,
                node_count: 1,
                warnings: vec![],
            })
            .into_response(),
            Err(e) => log_query_error(e),
        }
    }
}

/// `GET /v1/logs/sql?q=SELECT...` — query logs via DataFusion SQL.
pub(super) async fn logs_sql_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    // C3: this endpoint exposes the whole `logs` table. `LogStore::query`
    // filters by tenant; arbitrary SQL cannot be made to, so a scoped token
    // is refused rather than served another tenant's logs.
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Logs,
    )
    .await
    {
        return resp;
    }
    let Some(log_store) = &state.log_store else {
        return Json(serde_json::json!({"error": "log store not enabled"})).into_response();
    };

    let Some(sql) = params.get("q") else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing 'q' query parameter"})),
        )
            .into_response();
    };

    let store = log_store.read().await;
    // OBS5: bounded access — read-only, `logs`-table only, row- and
    // memory-capped. A rejected query is a 400, not a 500.
    match store.query_sql_json_bounded(sql).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e @ crate::ketchup::types::KetchupError::QueryRejected { .. }) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `POST /v1/logs/export` request body.
#[derive(serde::Deserialize)]
pub(super) struct LogsExportRequest {
    /// Where the Parquet files go — a path on the agent host, `file://`,
    /// `s3://` or `gs://`. Resolved agent-side, with the agent's credentials.
    pub(super) destination: String,
}

/// `POST /v1/logs/export` — export this node's Parquet log store now.
///
/// Serialises with periodic, pressure and offline exporters through the same
/// checkpoint lock. Success includes durable acknowledgement persistence.
pub(super) async fn logs_export_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Json(request): Json<LogsExportRequest>,
) -> Response {
    // Admin: this writes files wherever the destination points, using the
    // agent host's filesystem and object-store credentials.
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Admin,
    )
    .await
    {
        return resp;
    }
    let Some(log_store) = &state.log_store else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "log store not enabled"})),
        )
            .into_response();
    };
    let data_dir = log_store.read().await.data_dir().to_path_buf();
    let mut checkpoint = crate::ketchup::export::ExportCheckpoint::default();
    let node_id = state
        .node_name
        .clone()
        .unwrap_or_else(|| "local".to_string());

    match crate::ketchup::export::export_logs(
        &data_dir,
        &request.destination,
        &node_id,
        &mut checkpoint,
    )
    .await
    {
        Ok(result) => Json(serde_json::json!({
            "files_exported": result.files_exported,
            "bytes_written": result.bytes_written,
            "node_id": node_id,
            "checkpoint_saved": true,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
