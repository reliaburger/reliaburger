//! App lifecycle routes: stop, delete and exec.

use super::*;

/// Stop an app.
///
/// In cluster mode, stopping an app is a desired-state change: the app is
/// deleted from Raft (`AppDelete`) so the scheduler stops placing it and no
/// reconciler resurrects it on the next tick (DEP2). The local supervisor
/// stop is then best-effort. Because the delete goes through the council,
/// a leader that holds no local replica still clears cluster state instead
/// of returning a spurious 404. In standalone mode there is no desired
/// state, so we just stop the local instances as before.
pub(super) async fn stop_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    headers: HeaderMap,
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
        crate::config::PermissionAction::Scale,
        &app,
        &namespace,
    )
    .await
    {
        return resp;
    }

    if let Some(response) = crate::bun::job_api::stop_definition(
        &state,
        auth.as_deref(),
        &app,
        &namespace,
        false,
        &headers,
    )
    .await
    {
        return response;
    }
    let _gate = if state.council.is_none() {
        Some(state.task_arrays.apply_gate.clone().lock_owned().await)
    } else {
        None
    };
    if crate::bun::task_array_leader::read_task_arrays(&state)
        .await
        .deployment_owner(&app, &namespace)
        .is_some()
    {
        return (
            StatusCode::CONFLICT,
            "an unsettled deployment owns this workload",
        )
            .into_response();
    }
    if let Some(council) = state.council.clone() {
        return cluster_app_change(state, council, app, namespace, AppChange::Stop).await;
    }

    stop_local(&state, app, namespace).await
}

/// `POST /v1/delete/{app}/{namespace}` — remove an app from the cluster.
///
/// In cluster mode the app leaves desired state and every node retires its
/// instances. A standalone node has no desired state beyond its running
/// instances, so deleting is the same as stopping there.
pub(super) async fn delete_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    headers: HeaderMap,
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

    if let Some(response) = crate::bun::job_api::stop_definition(
        &state,
        auth.as_deref(),
        &app,
        &namespace,
        true,
        &headers,
    )
    .await
    {
        return response;
    }
    let _gate = if state.council.is_none() {
        Some(state.task_arrays.apply_gate.clone().lock_owned().await)
    } else {
        None
    };
    if crate::bun::task_array_leader::read_task_arrays(&state)
        .await
        .deployment_owner(&app, &namespace)
        .is_some()
    {
        return (
            StatusCode::CONFLICT,
            "an unsettled deployment owns this workload",
        )
            .into_response();
    }
    if let Some(council) = state.council.clone() {
        return cluster_app_change(state, council, app, namespace, AppChange::Delete).await;
    }

    stop_local(&state, app, namespace).await
}

/// Whether `relish stop` or `relish delete` is changing an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AppChange {
    /// Scale to zero, keeping the specification, until the next apply.
    Stop,
    /// Remove the app from desired state.
    Delete,
}

impl AppChange {
    pub(super) fn verb(self) -> &'static str {
        match self {
            AppChange::Stop => "stop",
            AppChange::Delete => "delete",
        }
    }
}

/// Stop or delete an app in cluster mode through Raft. Nodes' reconcilers
/// then retire its instances, the leader's own included. Stopping the local
/// replica directly used to leave the reconciler believing it still ran, so
/// an apply straight afterwards never brought it back.
pub(super) async fn cluster_app_change(
    state: ApiState,
    council: Arc<crate::council::CouncilNode>,
    app: String,
    namespace: String,
    change: AppChange,
) -> Response {
    // Followers can't write to Raft (openraft does not forward client
    // writes), so forward the whole request to the leader's API.
    if !council.is_leader().await {
        let Some(leader_url) = leader_api_url(&state, &council).await else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "no cluster leader known yet; retry shortly"
                })),
            )
                .into_response();
        };
        let url = format!("{leader_url}/v1/{}/{app}/{namespace}", change.verb());
        let mut request = state.cluster_http.client().post(url);
        if let Some(token) = &state.service_token {
            request = request.bearer_auth(token);
        }
        return match request.send().await {
            Ok(response) => {
                let status = StatusCode::from_u16(response.status().as_u16())
                    .unwrap_or(StatusCode::BAD_GATEWAY);
                let body = response.bytes().await.unwrap_or_default();
                (status, body).into_response()
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": format!("failed to forward {} to the leader: {e}", change.verb())
                })),
            )
                .into_response(),
        };
    }

    let app_id = crate::meat::AppId::new(&app, &namespace);
    let request = match change {
        AppChange::Stop => crate::council::types::RaftRequest::AppStop { app_id },
        AppChange::Delete => crate::council::types::RaftRequest::AppDelete { app_id },
    };
    match council.write(request).await {
        Ok(crate::council::CouncilResponse::Refused { reason }) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": reason })),
        )
            .into_response(),
        Ok(_) => {
            let status = match change {
                AppChange::Stop => "stopping",
                AppChange::Delete => "deleting",
            };
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "status": status })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("failed to update desired state: {e}")
            })),
        )
            .into_response(),
    }
}

/// Stop an app on this node only (standalone mode).
pub(super) async fn stop_local(state: &ApiState, app: String, namespace: String) -> Response {
    match ask_agent(&state.cmd_tx, |response| AgentCommand::Stop {
        app_name: app,
        namespace,
        response,
    })
    .await
    {
        Ok(Ok(())) => Json(serde_json::json!({ "status": "stopped" })).into_response(),
        Ok(Err(error)) => {
            let status = match error {
                crate::bun::BunError::AppNotFound { .. } => StatusCode::NOT_FOUND,
                crate::bun::BunError::WorkloadBusy { .. } => StatusCode::CONFLICT,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (
                status,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response()
        }
        Err(response) => response,
    }
}

/// Request body for the exec endpoint.
#[derive(Deserialize)]
pub(super) struct ExecRequest {
    pub(super) command: Vec<String>,
}

/// Execute a command inside a running instance.
pub(super) async fn exec_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Json(body): Json<ExecRequest>,
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
        crate::config::PermissionAction::Exec,
        &app,
        &namespace,
    )
    .await
    {
        return resp;
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::Exec {
        app_name: app,
        namespace,
        command: body.command,
        response,
    })
    .await
    {
        Ok(Ok(output)) => Json(serde_json::json!({ "output": output })).into_response(),
        Ok(Err(e)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(response) => response,
    }
}
