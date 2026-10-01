//! Status routes: apps, instance status (local and cluster-wide), top,
//! jobs and the event stream.

use super::*;

/// List all instances.
/// `GET /v1/apps` — the currently deployed resources in the CLI plan's
/// identifier format, for `relish apply --dry-run` diffing.
///
/// Cluster mode answers from the council's desired state (authoritative and
/// cluster-wide: apps with images, declared namespaces and permissions),
/// merged over the local agent's view (which contributes node-local jobs —
/// jobs don't live in desired state). Standalone answers from the local
/// agent alone.
pub(super) async fn current_apps_handler(State(state): State<ApiState>) -> Response {
    // Plan-key → image; later inserts overwrite, so the council's
    // authoritative entries land last.
    let mut resources: std::collections::BTreeMap<String, Option<String>> =
        std::collections::BTreeMap::new();

    if let Ok(local) = ask_agent(&state.cmd_tx, |response| AgentCommand::CurrentResources {
        response,
    })
    .await
    {
        for entry in local {
            resources.insert(entry.resource, entry.image);
        }
    }

    if let Some(council) = &state.council {
        let desired = council.desired_state().await;
        for (app_id, spec) in &desired.apps {
            resources.insert(format!("app.{}", app_id.name), spec.image.clone());
        }
        for name in desired.namespaces.keys() {
            resources.insert(format!("namespace.{name}"), None);
        }
        for name in desired.permissions.keys() {
            resources.insert(format!("permission.{name}"), None);
        }
    }

    let rows: Vec<crate::bun::agent::CurrentResourceStatus> = resources
        .into_iter()
        .map(|(resource, image)| crate::bun::agent::CurrentResourceStatus { resource, image })
        .collect();
    Json(rows).into_response()
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
    let limit = query.limit.unwrap_or(100);
    let local = match &state.events {
        Some(events) => events
            .read()
            .await
            .recent(limit, query.app.as_deref(), query.severity),
        None => Vec::new(),
    };
    let mut answers = vec![(local_node_name(&state), local)];
    let mut warnings = Vec::new();
    if !query.local {
        let path = peer_events_path(&query, limit);
        let (peers, failures) = fan_out_to_peers::<crate::bun::cluster_view::ClusterEvents>(
            &state,
            &path,
            CLUSTER_STATUS_TIMEOUT,
        )
        .await;
        answers.extend(peers.into_iter().map(|(node, view)| (node, view.events)));
        warnings = failures;
    }
    Json(crate::bun::cluster_view::ClusterEvents {
        events: crate::bun::cluster_view::merge_events(answers, limit),
        warnings,
    })
    .into_response()
}

/// Upgrade an authenticated request to the live event stream.
pub(super) async fn ws_events_handler(
    State(state): State<ApiState>,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade
        .on_upgrade(move |socket| ws_events_session(socket, state.events))
        .into_response()
}

pub(super) async fn ws_events_session(
    mut socket: WebSocket,
    events: Option<Arc<RwLock<crate::bun::events::EventStore>>>,
) {
    let Some(events) = events else { return };
    let (recent, mut receiver) = {
        let store = events.read().await;
        (store.recent(50, None, None), store.subscribe())
    };
    for event in recent {
        let Ok(json) = serde_json::to_string(&event) else {
            continue;
        };
        if socket.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            event = receiver.recv() => match event {
                Ok(event) => {
                    let Ok(json) = serde_json::to_string(&event) else { continue };
                    if socket.send(Message::Text(json.into())).await.is_err() { return; }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            message = socket.recv() => if message.is_none() { return; },
        }
    }
}

/// Status for a specific app.
pub(super) async fn status_app_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    match local_statuses(&state).await.map_err(unavailable_response) {
        Ok(statuses) => {
            let filtered: Vec<&InstanceStatus> = statuses
                .iter()
                .filter(|s| s.app_name == app && s.namespace == namespace)
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
