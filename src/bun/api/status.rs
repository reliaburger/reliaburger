//! Status routes: apps, instance status (local and cluster-wide), top,
//! jobs and the event stream.

use super::*;

/// List all instances.
/// `GET /v1/apps` — the currently deployed resources in the CLI plan's
/// identifier format, for `relish apply --dry-run` diffing.
///
/// Cluster mode answers from the council's desired state (authoritative and
/// cluster-wide: complete app, namespace and permission specifications),
/// merged over the local agent's view (which contributes node-local jobs —
/// jobs don't live in desired state). Standalone answers from the local
/// agent alone.
pub(super) async fn current_apps_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
) -> Response {
    use crate::bun::agent::CurrentResourceStatus;
    use crate::config::fingerprint::{app_fingerprint_in, app_resource_key, spec_fingerprint};
    let mut resources = std::collections::BTreeMap::<String, CurrentResourceStatus>::new();
    let local = match ask_agent(&state.cmd_tx, |response| AgentCommand::CurrentResources {
        response,
    })
    .await
    {
        Ok(local) => local,
        Err(_) => return agent_unavailable(),
    };
    for entry in local {
        resources.insert(entry.resource.clone(), entry);
    }
    if let Some(council) = &state.council {
        let desired = council.desired_state().await;
        for (app_id, spec) in &desired.apps {
            let resource = app_resource_key(&app_id.name, &app_id.namespace);
            resources.insert(
                resource.clone(),
                CurrentResourceStatus {
                    resource,
                    image: spec.image.clone(),
                    fingerprint: app_fingerprint_in(spec, &app_id.namespace),
                },
            );
        }
        for (name, spec) in &desired.namespaces {
            let resource = format!("namespace.{name}");
            resources.insert(
                resource.clone(),
                CurrentResourceStatus {
                    resource,
                    image: None,
                    fingerprint: spec_fingerprint(spec),
                },
            );
        }
        for (name, spec) in &desired.permissions {
            let resource = format!("permission.{name}");
            resources.insert(
                resource.clone(),
                CurrentResourceStatus {
                    resource,
                    image: None,
                    fingerprint: spec_fingerprint(spec),
                },
            );
        }
    }
    let rows: Vec<_> = resources
        .into_values()
        .filter(|row| current_resource_visible(&row.resource, auth.as_deref()))
        .collect();
    Json(rows).into_response()
}

/// The preview endpoint must retain the caller's resource scope as status does.
fn current_resource_visible(
    resource: &str,
    auth: Option<&crate::sesame::auth::AuthContext>,
) -> bool {
    let Some((kind, label)) = resource.split_once('.') else {
        return false;
    };
    match kind {
        "app" | "job" => {
            let (namespace, name) = label.split_once('/').unwrap_or(("default", label));
            crate::sesame::auth::authorize_scoped(auth, name, namespace).is_ok()
        }
        "namespace" => auth
            .and_then(|context| context.scoped_namespaces.as_ref())
            .is_none_or(|namespaces| namespaces.iter().any(|namespace| namespace == label)),
        "permission" => auth.is_none_or(|context| {
            (context.scoped_namespaces.is_none() && context.scoped_apps.is_none())
                || context.token_name == label
        }),
        _ => false,
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct StatusQuery {
    #[serde(default)]
    pub(super) cluster: bool,
}

pub(super) async fn status_handler(
    State(state): State<ApiState>,
    Query(query): Query<StatusQuery>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
) -> Response {
    let auth = auth.as_deref();
    let visible = |app: &str, namespace: &str| {
        crate::sesame::auth::authorize_scoped(auth, app, namespace).is_ok()
    };
    if !query.cluster {
        return match local_statuses(&state).await {
            Ok(mut statuses) => {
                statuses.retain(|status| visible(&status.app_name, &status.namespace));
                Json(statuses).into_response()
            }
            Err(error) => unavailable_response(error),
        };
    }
    // Peers answer the fan-out under this node's service token, which sees
    // everything, so the caller's scope has to be applied here.
    match cluster_statuses(&state).await {
        Ok(mut statuses) => {
            statuses
                .retain(|status| visible(&status.instance.app_name, &status.instance.namespace));
            Json(statuses).into_response()
        }
        Err(error) => unavailable_response(error),
    }
}

pub(super) async fn cluster_statuses(
    state: &ApiState,
) -> Result<Vec<crate::bun::agent::ClusterInstanceStatus>, String> {
    let (statuses, failures) = collect_cluster_statuses(state, CLUSTER_STATUS_TIMEOUT).await?;
    match failures.into_iter().next() {
        Some(failure) => Err(format!("status incomplete: {failure}")),
        None => Ok(statuses),
    }
}

/// Every node's workload statuses, plus one message per peer that didn't
/// answer. Only this node's own status failing is an error: callers decide
/// whether a partial cluster view is good enough.
pub(super) async fn collect_cluster_statuses(
    state: &ApiState,
    peer_timeout: std::time::Duration,
) -> Result<(Vec<crate::bun::agent::ClusterInstanceStatus>, Vec<String>), String> {
    let local_name = local_node_name(state);
    let local = local_statuses(state).await?;
    let (peers, failures) =
        fan_out_to_peers::<Vec<InstanceStatus>>(state, "/v1/status", peer_timeout).await;
    let mut statuses: Vec<_> = std::iter::once((local_name, local))
        .chain(peers)
        .flat_map(|(node, instances)| {
            instances
                .into_iter()
                .map(move |instance| crate::bun::agent::ClusterInstanceStatus {
                    node: node.clone(),
                    instance,
                })
        })
        .collect();
    statuses.sort_by(|left, right| {
        (&left.node, &left.instance.namespace, &left.instance.id).cmp(&(
            &right.node,
            &right.instance.namespace,
            &right.instance.id,
        ))
    });
    Ok((statuses, failures))
}

/// `GET /v1/top[?cluster=true]`: workloads with their latest CPU and memory.
///
/// Without `cluster` a node answers for itself. With it, the node merges its
/// own rows with every peer's; a peer that doesn't answer becomes a warning
/// rather than failing the whole view, so `relish top` still works while a
/// node is down.
pub(super) async fn top_handler(
    State(state): State<ApiState>,
    Query(query): Query<StatusQuery>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
) -> Response {
    let auth = auth.as_deref();
    // CPU and memory are metrics, so a `[permission]` spec must grant
    // `metrics` on a row's app for the caller to see it (B18).
    let permissions = permission_map(&state).await;
    let visible = |row: &crate::bun::top::TopRow| {
        let (app, namespace) = (&row.instance.app_name, &row.instance.namespace);
        crate::sesame::auth::authorize_scoped(auth, app, namespace).is_ok()
            && crate::sesame::auth::authorize_permission(
                auth,
                crate::config::PermissionAction::Metrics,
                app,
                namespace,
                &permissions,
            )
            .is_ok()
    };
    let mut rows = match local_top_rows(&state).await {
        Ok(rows) => rows,
        Err(error) => return unavailable_response(error),
    };
    if !query.cluster {
        rows.retain(visible);
        return Json(rows).into_response();
    }
    let (peers, warnings) =
        fan_out_to_peers::<Vec<crate::bun::top::TopRow>>(&state, "/v1/top", CLUSTER_STATUS_TIMEOUT)
            .await;
    rows.extend(peers.into_iter().flat_map(|(_, peer_rows)| peer_rows));
    // Peers answered with the node's service token, which sees everything,
    // so the caller's scope applies here.
    rows.retain(visible);
    rows.sort_by(|left, right| {
        (&left.node, &left.instance.namespace, &left.instance.id).cmp(&(
            &right.node,
            &right.instance.namespace,
            &right.instance.id,
        ))
    });
    Json(crate::bun::top::ClusterTop { rows, warnings }).into_response()
}

/// This node's workloads joined to their latest samples in its own store.
pub(super) async fn local_top_rows(
    state: &ApiState,
) -> Result<Vec<crate::bun::top::TopRow>, String> {
    use crate::bun::top::{CPU_METRIC, MEMORY_METRIC, USAGE_WINDOW_SECS};

    let statuses = local_statuses(state).await?;
    let usage = match &state.mayo {
        Some(mayo) => {
            let since = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .saturating_sub(USAGE_WINDOW_SECS);
            // Missing samples leave the columns empty; they don't hide the
            // workloads themselves.
            match mayo
                .read()
                .await
                .query_names_since(&[CPU_METRIC, MEMORY_METRIC], since)
                .await
            {
                Ok(samples) => crate::bun::top::latest_usage(&samples),
                Err(_) => std::collections::HashMap::new(),
            }
        }
        None => std::collections::HashMap::new(),
    };
    Ok(crate::bun::top::node_rows(
        &local_node_name(state),
        statuses,
        &usage,
    ))
}

/// List all run-to-completion workload instances.
///
/// `?cluster=true` merges every live member's jobs, each tagged with its
/// node, and names any member that didn't answer. Either way the rows are
/// trimmed to the caller's token scope.
pub(super) async fn jobs_handler(
    State(state): State<ApiState>,
    Query(query): Query<StatusQuery>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
) -> Response {
    let auth = auth.as_deref();
    let visible = |job: &crate::bun::agent::JobStatus| {
        crate::sesame::auth::authorize_scoped(auth, &job.name, &job.namespace).is_ok()
    };
    let Ok(mut local) = ask_agent(&state.cmd_tx, |response| AgentCommand::JobStatus {
        response,
    })
    .await
    else {
        return agent_unavailable();
    };
    if !query.cluster {
        local.retain(visible);
        return Json(local).into_response();
    }
    let (peers, warnings) = fan_out_to_peers::<Vec<crate::bun::agent::JobStatus>>(
        &state,
        "/v1/jobs",
        CLUSTER_STATUS_TIMEOUT,
    )
    .await;
    let mut jobs = crate::bun::cluster_view::merge_jobs(
        std::iter::once((local_node_name(&state), local))
            .chain(peers)
            .collect(),
    );
    // Peers answered with the service token, which sees every namespace.
    jobs.retain(|job| visible(&job.row));
    Json(crate::bun::cluster_view::ClusterJobs { jobs, warnings }).into_response()
}

#[derive(Deserialize)]
pub(super) struct EventsQuery {
    pub(super) limit: Option<usize>,
    pub(super) app: Option<String>,
    pub(super) severity: Option<crate::bun::events::EventSeverity>,
    /// Answer from this node's store only. Set on the fan-out's own requests
    /// so a peer never fans out again.
    #[serde(default)]
    pub(super) local: bool,
    /// Stream this node's new events over SSE instead of listing. The feed a
    /// cluster-wide live stream reads from each member; always this node's
    /// own events.
    #[serde(default)]
    pub(super) follow: bool,
}

/// The request a peer gets for its share of `/v1/events`: the same filters,
/// answered from its own store.
//
// A plain function rather than inline in the handler because the URL
// serializer holds a non-`Send` reference; kept out of the async body it
// can't make the handler's future un-`Send`.
pub(super) fn peer_events_path(query: &EventsQuery, limit: usize) -> String {
    let mut params = url::form_urlencoded::Serializer::new(String::new());
    params.append_pair("limit", &limit.to_string());
    params.append_pair("local", "true");
    if let Some(app) = &query.app {
        params.append_pair("app", app);
    }
    if let Some(severity) = query.severity
        && let Ok(serde_json::Value::String(severity)) = serde_json::to_value(severity)
    {
        params.append_pair("severity", &severity);
    }
    format!("/v1/events?{}", params.finish())
}

/// Return the newest events across the cluster, oldest first.
///
/// Each node keeps its own bounded store, so the node asked merges its own
/// with every live member's and names any member that didn't answer.
pub(super) async fn events_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Query(query): Query<EventsQuery>,
) -> Response {
    // Audit events span every app and namespace, so a scoped token is refused
    // (C3) just as it is for the cluster-wide metrics and logs endpoints.
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
    if query.follow {
        let (events_tx, events_rx) = mpsc::channel(256);
        let Some(relay) = spawn_local_event_relay(&state, events_tx).await else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let stream = ReceiverStream::new(events_rx).filter_map(|event| async move {
            let json = serde_json::to_string(&event).ok()?;
            Some(Ok::<_, std::convert::Infallible>(
                Event::default().data(json),
            ))
        });
        // The relay ends when the client goes: its sender's receiver is
        // dropped with the response stream.
        drop(relay);
        return Sse::new(stream)
            .keep_alive(axum::response::sse::KeepAlive::default())
            .into_response();
    }
    let limit = query.limit.unwrap_or(100);
    Json(cluster_recent_events(&state, &query, limit).await).into_response()
}

/// The newest `limit` events matching `query`, from this node and, unless
/// the query is `local`, every live member, oldest first.
async fn cluster_recent_events(
    state: &ApiState,
    query: &EventsQuery,
    limit: usize,
) -> crate::bun::cluster_view::ClusterEvents {
    let local = match &state.events {
        Some(events) => events
            .read()
            .await
            .recent(limit, query.app.as_deref(), query.severity),
        None => Vec::new(),
    };
    let mut answers = vec![(local_node_name(state), local)];
    let mut warnings = Vec::new();
    if !query.local {
        let path = peer_events_path(query, limit);
        let (peers, failures) = fan_out_to_peers::<crate::bun::cluster_view::ClusterEvents>(
            state,
            &path,
            CLUSTER_STATUS_TIMEOUT,
        )
        .await;
        answers.extend(peers.into_iter().map(|(node, view)| (node, view.events)));
        warnings = failures;
    }
    crate::bun::cluster_view::ClusterEvents {
        events: crate::bun::cluster_view::merge_events(answers, limit),
        warnings,
    }
}

/// Copy this node's new events into `events`, each tagged with this node's
/// name, until `events` closes. `None` when the node keeps no events.
/// Subscribes before it returns, so nothing recorded afterwards is missed.
async fn spawn_local_event_relay(
    state: &ApiState,
    events: mpsc::Sender<crate::bun::events::ClusterEvent>,
) -> Option<tokio::task::AbortHandle> {
    let mut receiver = state.events.as_ref()?.read().await.subscribe();
    let node = local_node_name(state);
    Some(
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = events.closed() => return,
                    event = receiver.recv() => match event {
                        Ok(mut event) => {
                            event.node.get_or_insert_with(|| node.clone());
                            if events.send(event).await.is_err() {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    },
                }
            }
        })
        .abort_handle(),
    )
}

/// Relay every other live member's new events into `events` until it
/// closes. Every [`super::logs::LOG_FOLLOW_REFRESH`] it re-reads the
/// membership, opens a feed to each member it isn't reading yet, and drops
/// the feeds of members that left; a feed that ends is reopened on the next
/// pass.
async fn relay_peer_events(
    state: ApiState,
    events: mpsc::Sender<crate::bun::events::ClusterEvent>,
) {
    let Some(membership) = state.membership.clone() else {
        return;
    };
    let self_name = local_node_name(&state);
    let mut feeds: std::collections::HashMap<String, tokio::task::AbortHandle> =
        std::collections::HashMap::new();
    loop {
        let members = membership.read().await.clone();
        feeds.retain(|node, feed| {
            let alive = members.iter().any(|member| &member.node_id.0 == node);
            if !alive {
                feed.abort();
            }
            alive && !feed.is_finished()
        });
        for member in members {
            let node = member.node_id.0.clone();
            if node == self_name || feeds.contains_key(&node) {
                continue;
            }
            let url = state.cluster_http.url(
                &member.address.to_string(),
                "/v1/events?follow=true&local=true",
            );
            let mut request = state.cluster_http.client().get(url);
            if let Some(token) = &state.service_token {
                request = request.bearer_auth(token);
            }
            let events = events.clone();
            let name = node.clone();
            let feed = tokio::spawn(async move {
                let _ = relay_one_peer_feed(request, &name, &events).await;
            });
            feeds.insert(node, feed.abort_handle());
        }
        tokio::select! {
            () = events.closed() => break,
            () = tokio::time::sleep(super::logs::LOG_FOLLOW_REFRESH) => {}
        }
    }
    for feed in feeds.into_values() {
        feed.abort();
    }
}

/// Read one member's SSE feed of new events into `events`, tagging each
/// with `node` when the member didn't.
async fn relay_one_peer_feed(
    request: reqwest::RequestBuilder,
    node: &str,
    events: &mpsc::Sender<crate::bun::events::ClusterEvent>,
) -> Result<(), String> {
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), request.send())
        .await
        .map_err(|_| format!("node {node}: event feed did not start within 5s"))?
        .map_err(|error| format!("node {node}: event feed failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "node {node}: event feed refused: {}",
            response.status()
        ));
    }
    let mut decoder = crate::ketchup::sse::SseDecoder::default();
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| format!("node {node}: event feed broke: {error}"))?;
        for frame in decoder.push(&chunk) {
            let Ok(mut event) =
                serde_json::from_str::<crate::bun::events::ClusterEvent>(&frame.data)
            else {
                continue;
            };
            event.node.get_or_insert_with(|| node.to_string());
            if events.send(event).await.is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Upgrade an authenticated request to the live event stream.
pub(super) async fn ws_events_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    upgrade: WebSocketUpgrade,
) -> Response {
    // The same events as `/v1/events`, live: they span every namespace, so a
    // scoped token is refused before the upgrade, as it is there (C3).
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
    upgrade
        .on_upgrade(move |socket| ws_events_session(socket, state))
        .into_response()
}

/// How many recent events a live stream opens with.
const LIVE_EVENTS_BACKLOG: usize = 50;

/// The live event stream: the cluster's recent events, oldest first, then
/// every member's new ones as they happen, each tagged with its node.
///
/// This node's feed subscribes before the backlog is read, so an event that
/// lands in between arrives in both; it's dropped from the live half by its
/// node, sequence and time. A member's events are relayed from its own
/// `follow=true&local=true` feed, so a peer never fans out again.
pub(super) async fn ws_events_session(mut socket: WebSocket, state: ApiState) {
    let (events_tx, mut events_rx) = mpsc::channel::<crate::bun::events::ClusterEvent>(256);
    let Some(local_feed) = spawn_local_event_relay(&state, events_tx.clone()).await else {
        return;
    };
    let backlog_query = EventsQuery {
        limit: Some(LIVE_EVENTS_BACKLOG),
        app: None,
        severity: None,
        local: false,
        follow: false,
    };
    let backlog = cluster_recent_events(&state, &backlog_query, LIVE_EVENTS_BACKLOG)
        .await
        .events;
    // Sequences are per process, so a member that restarts reuses them; the
    // timestamp keeps its new events from passing as the backlog's.
    let identity = |event: &crate::bun::events::ClusterEvent| {
        (event.node.clone(), event.sequence, event.timestamp)
    };
    let sent: std::collections::HashSet<_> = backlog.iter().map(identity).collect();
    let peers = tokio::spawn(relay_peer_events(state.clone(), events_tx));
    let send = |event: &crate::bun::events::ClusterEvent| {
        serde_json::to_string(event)
            .ok()
            .map(|json| Message::Text(json.into()))
    };
    'session: {
        for event in &backlog {
            if let Some(message) = send(event)
                && socket.send(message).await.is_err()
            {
                break 'session;
            }
        }
        loop {
            tokio::select! {
                event = events_rx.recv() => {
                    let Some(event) = event else { break 'session };
                    if sent.contains(&identity(&event)) {
                        continue;
                    }
                    if let Some(message) = send(&event)
                        && socket.send(message).await.is_err()
                    {
                        break 'session;
                    }
                }
                message = socket.recv() => if message.is_none() { break 'session; },
            }
        }
    }
    local_feed.abort();
    peers.abort();
}

/// Status for a specific app.
pub(super) async fn status_app_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    let selection = match super::logs::resolve_log_path(&state, &app, &namespace, None).await {
        Ok(selection) => selection,
        Err(response) => return response,
    };
    let app = selection.logical_name;

    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    match local_statuses(&state).await.map_err(unavailable_response) {
        Ok(statuses) => {
            let filtered: Vec<&InstanceStatus> = statuses
                .iter()
                .filter(|s| {
                    s.app_name == app
                        && s.namespace == namespace
                        && selection
                            .selected_instance
                            .as_ref()
                            .is_none_or(|id| &s.id == id)
                })
                .collect();
            if filtered.is_empty() {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": format!("app {app} not found in {namespace}") })),
                )
                    .into_response()
            } else {
                Json(serde_json::json!(filtered)).into_response()
            }
        }
        Err(response) => response,
    }
}
