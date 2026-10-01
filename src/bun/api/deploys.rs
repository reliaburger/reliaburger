//! Deploy routes: cancel, active and recorded operations, history and
//! rollback.

use super::*;

/// Request node-local cooperative cancellation under the same authority as apply.
pub(super) async fn deploy_cancel_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return response;
    }
    let snapshot = match deploy_operation_snapshot(&state).await {
        Ok(snapshot) => snapshot,
        Err(response) => return response,
    };
    let Some(operation) = snapshot
        .active_deploys
        .iter()
        .chain(&snapshot.history)
        .find(|operation| operation.id.as_str() == id)
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "deploy operation not found on this node"})),
        )
            .into_response();
    };
    let permissions = permission_map(&state).await;
    for target in &operation.targets {
        if let Err(response) =
            crate::sesame::auth::authorize_scoped(auth.as_deref(), &target.name, &target.namespace)
        {
            return response;
        }
        if let Err(response) = crate::sesame::auth::authorize_permission(
            auth.as_deref(),
            crate::config::PermissionAction::Deploy,
            &target.name,
            &target.namespace,
            &permissions,
        ) {
            return response;
        }
    }
    let (response, result) = oneshot::channel();
    let request = async {
        state
            .cmd_tx
            .send(AgentCommand::CancelDeploy {
                operation_id: id.into(),
                response,
            })
            .await
            .ok()?;
        result.await.ok()
    };
    match tokio::time::timeout(std::time::Duration::from_secs(2), request).await {
        Ok(Some(Some(operation))) => {
            let status = if operation.outcome.is_some() {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            (status, Json(operation)).into_response()
        }
        Ok(Some(None)) => {
            (StatusCode::NOT_FOUND, "deploy operation no longer retained").into_response()
        }
        Ok(None) => (StatusCode::SERVICE_UNAVAILABLE, "agent unavailable").into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "cancellation receipt unknown; query or retry the same operation ID",
        )
            .into_response(),
    }
}

/// `GET /v1/deploys/active` — list active deploys.
pub(super) async fn deploys_active_handler(State(state): State<ApiState>) -> Response {
    match deploy_operation_snapshot(&state).await {
        Ok(snapshot) => Json(crate::bun::deploy_operations::ActiveDeployOperations {
            active_deploys: snapshot.active_deploys,
        })
        .into_response(),
        Err(response) => response,
    }
}

/// `GET /v1/deploys/operations` — active operations and bounded recent
/// terminal history, using the same stable record shape for both.
pub(super) async fn deploys_operations_handler(State(state): State<ApiState>) -> Response {
    match deploy_operation_snapshot(&state).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(response) => response,
    }
}

// `Response` is large but it IS the HTTP reply to send on failure —
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
pub(super) async fn deploy_operation_snapshot(
    state: &ApiState,
) -> Result<crate::bun::deploy_operations::DeployOperationSnapshot, Response> {
    let (response, result) = oneshot::channel();
    state
        .cmd_tx
        .send(AgentCommand::DeployOperations { response })
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "agent unavailable"})),
            )
                .into_response()
        })?;
    tokio::time::timeout(std::time::Duration::from_secs(2), result)
        .await
        .map_err(|_| {
            (
                StatusCode::GATEWAY_TIMEOUT,
                Json(serde_json::json!({"error": "agent deploy-state query timed out"})),
            )
                .into_response()
        })?
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "agent unavailable"})),
            )
                .into_response()
        })
}

#[derive(Deserialize)]
pub(super) struct NamespaceQuery {
    pub(super) namespace: Option<String>,
    /// Answer from this node's records only (set on fan-out requests).
    #[serde(default)]
    pub(super) local: bool,
}

/// An app's deploy history across the cluster.
///
/// Every node records its own rollout of the replicas placed on it, so the
/// full history is the merge of every live member's records. `local_only`
/// answers with this node's records alone, as a peer does for a fan-out.
pub(super) async fn cluster_deploy_history(
    state: &ApiState,
    app: &str,
    namespace: &str,
    local_only: bool,
) -> crate::bun::cluster_view::ClusterDeployHistory {
    let local: Vec<DeployHistoryEntry> = match &state.deploy_history {
        Some(history) => history
            .read()
            .await
            .iter()
            .filter(|entry| entry.app_id.name == app && entry.app_id.namespace == namespace)
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    let mut answers = vec![(local_node_name(state), local)];
    let mut warnings = Vec::new();
    if !local_only {
        let app_segment: String = url::form_urlencoded::byte_serialize(app.as_bytes()).collect();
        let namespace_value: String =
            url::form_urlencoded::byte_serialize(namespace.as_bytes()).collect();
        let path =
            format!("/v1/deploys/history/{app_segment}?namespace={namespace_value}&local=true");
        let (peers, failures) = fan_out_to_peers::<crate::bun::cluster_view::ClusterDeployHistory>(
            state,
            &path,
            CLUSTER_STATUS_TIMEOUT,
        )
        .await;
        answers.extend(peers.into_iter().map(|(node, view)| {
            let entries = view.history.into_iter().map(|entry| entry.row).collect();
            (node, entries)
        }));
        warnings = failures;
    }
    crate::bun::cluster_view::ClusterDeployHistory {
        app: app.to_string(),
        namespace: namespace.to_string(),
        history: crate::bun::cluster_view::merge_deploy_history(answers),
        warnings,
    }
}

/// `GET /v1/deploys/history/{app}` — deploy history for an app, from every
/// node, each entry tagged with the node that recorded it.
///
/// The namespace rides in as a query parameter rather than a path segment
/// so the route (and every client bookmarking it) keeps its shape. It
/// matters for more than tidiness: since DEP1 two apps of the same name can
/// coexist in different namespaces, so filtering on the bare name returned
/// both tenants' history to whoever asked (C3).
pub(super) async fn deploys_history_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(app): Path<String>,
    Query(query): Query<NamespaceQuery>,
) -> Response {
    let namespace = query.namespace.as_deref().unwrap_or("default");
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, namespace) {
        return resp;
    }
    Json(cluster_deploy_history(&state, &app, namespace, query.local).await).into_response()
}

/// `POST /v1/rollback/{app}/{namespace}` — redeploy the app's previous
/// successful spec (X3).
///
/// "Previous" means the last-but-one distinct successful deploy: the
/// most recent completed entry is the *current* version, so rollback
/// targets the one before it. Re-applies through the same path as
/// `apply` (Raft in cluster mode, local deploy otherwise).
pub(super) async fn rollback_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Deploy,
        &app,
        &namespace,
    )
    .await
    {
        return resp;
    }
    let Some(history) = &state.deploy_history else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "deploy history unavailable"})),
        )
            .into_response();
    };

    // Successful deploys for this app, newest first, that carry a spec.
    let target_spec = {
        let all = history.read().await;
        let mut successful: Vec<&DeployHistoryEntry> = all
            .iter()
            .filter(|e| {
                e.app_id.name == app
                    && e.app_id.namespace == namespace
                    && e.result == crate::meat::deploy_types::DeployResult::Completed
                    && e.spec.is_some()
            })
            .collect();
        successful.reverse(); // newest first
        // [0] is the current version; [1] is the rollback target.
        successful.get(1).and_then(|e| e.spec.clone()).map(|s| *s)
    };

    let Some(spec) = target_spec else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("no previous successful deploy to roll {app} back to")
            })),
        )
            .into_response();
    };

    // Re-apply the previous spec through the standard deploy path.
    let mut config = Config::default();
    config.app.insert(app.clone(), spec);
    let raw = toml::to_string(&config).unwrap_or_default();

    if let Some(council) = &state.council {
        return cluster_apply(
            state.clone(),
            Arc::clone(council),
            config,
            raw,
            None,
            HeaderMap::new(),
            None,
        )
        .await;
    }

    let (event_tx, event_rx) = mpsc::channel::<ApplyEvent>(32);
    if state
        .cmd_tx
        .send(AgentCommand::Deploy {
            config,
            events: event_tx,
        })
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "agent unavailable"})),
        )
            .into_response();
    }
    let stream = ReceiverStream::new(event_rx).map(|e| {
        Ok::<_, std::convert::Infallible>(
            Event::default().data(serde_json::to_string(&e).unwrap_or_default()),
        )
    });
    Sse::new(stream).into_response()
}
