//! App lifecycle routes: stop, delete, release and exec.

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
        return cluster_app_change(
            state,
            council,
            app,
            namespace,
            AppChange::Stop,
            auth.as_deref(),
            &headers,
        )
        .await;
    }

    stop_local(&state, app, namespace).await
}

/// `?force=true` on a delete or rollback: change a GitOps-managed app anyway.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ForceQuery {
    #[serde(default)]
    pub(super) force: bool,
}

/// `POST /v1/delete/{app}/{namespace}` — remove an app from the cluster.
///
/// In cluster mode the app leaves desired state and every node retires its
/// instances. A standalone node has no desired state beyond its running
/// instances, so deleting is the same as stopping there. An app GitOps
/// manages is refused without `?force=true`: the next sync would put it
/// back (#693).
pub(super) async fn delete_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(query): Query<ForceQuery>,
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
        let change = AppChange::Delete { force: query.force };
        return cluster_app_change(
            state,
            council,
            app,
            namespace,
            change,
            auth.as_deref(),
            &headers,
        )
        .await;
    }

    stop_local(&state, app, namespace).await
}

/// `POST /v1/release/{app}/{namespace}`: clear an app's `owner`, handing
/// it back to `relish apply` and GitOps (#1125). Only the owner's token or
/// an admin may release an app. Owners live in the council, so a standalone
/// node has nothing to release.
pub(super) async fn release_handler(
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
    let Some(council) = state.council.clone() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "app owners live in a cluster's council" })),
        )
            .into_response();
    };
    cluster_app_change(
        state,
        council,
        app,
        namespace,
        AppChange::Release,
        auth.as_deref(),
        &headers,
    )
    .await
}

/// The body of `POST /v1/scale/{app}/{namespace}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ScaleRequest {
    /// The new replica count, at least 1.
    replicas: u32,
    /// The count the caller saw in the app's spec, as `kubectl scale
    /// --current-replicas` takes it: when the spec has another count now,
    /// the scale is refused rather than overwriting whoever changed it.
    /// `None` skips the check.
    #[serde(default)]
    current_replicas: Option<u32>,
}

/// `POST /v1/scale/{app}/{namespace}`: set an app's replica count (#2176).
///
/// The count goes into the app's spec in the council, through the same
/// path as `relish apply` (`cluster_apply`): policy, owners, agent leases,
/// the audit record and the Raft write are all the apply's. The rest of
/// the spec is the one the council holds, so the scale changes nothing
/// else, and the image stays the bytes it was bound to. It isn't an
/// override: the next apply of the app's file puts back the file's count,
/// and a plan against the file shows the difference until then.
///
/// Refused: zero (that's `relish stop`), an autoscaled app, a daemon set,
/// an app GitOps manages without `?force=true`, and a `current_replicas`
/// that isn't the count now. A follower forwards the whole request to the
/// leader, which reads the spec from its own state. A standalone node has
/// no desired state to change.
pub(super) async fn scale_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    binder: Option<axum::Extension<crate::pickle::binding::ImageBinder>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(query): Query<ForceQuery>,
    headers: HeaderMap,
    body: String,
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
    // Parsed after the checks above, so a caller who may not scale the app
    // learns that, not what's wrong with their body.
    let request: ScaleRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(error) => {
            return scale_refused(
                StatusCode::BAD_REQUEST,
                format!(
                    "the body is {{\"replicas\": N}}, optionally with \"current_replicas\": {error}"
                ),
            );
        }
    };
    let Some(council) = state.council.clone() else {
        return scale_refused(
            StatusCode::BAD_REQUEST,
            "a standalone node keeps no desired state to scale; change `replicas` in the \
             app's file and run `relish apply`"
                .to_string(),
        );
    };
    if !crate::bun::leadership::leads(&council).await {
        return super::deploys::forward_streamed_to_leader(
            &state,
            &council,
            super::deploys::StreamedChange {
                verb: "scale",
                app: &app,
                namespace: &namespace,
                force: query.force,
                body,
            },
            &headers,
        )
        .await;
    }
    let app_id = crate::meat::AppId::new(&app, &namespace);
    if let Some(refusal) =
        super::apply::owner_command_refusal(&council, auth.as_deref(), &app_id, "scale").await
    {
        return refusal;
    }
    if let Some(refusal) =
        gitops_managed_refusal(&council, &app, &namespace, "scale", query.force).await
    {
        return refusal;
    }
    let config = match scaled_config(
        &council.desired_state().await,
        &app_id,
        request.replicas,
        request.current_replicas,
    ) {
        Ok(config) => config,
        Err((status, message)) => return scale_refused(status, message),
    };
    let raw = toml::to_string(&config).unwrap_or_default();
    cluster_apply(
        state.clone(),
        council,
        config,
        raw,
        None,
        headers,
        None,
        binder.map(|extension| extension.0),
        auth.map(|extension| extension.0),
    )
    .await
}

/// The one-app config a scale applies: the app's stored spec with
/// `replicas` in place of its count, or why there's none. The stored spec
/// is the effective one, images bound to digests, so re-applying it runs
/// the same bytes; a tag isn't resolved again.
fn scaled_config(
    desired: &crate::council::types::DesiredState,
    app_id: &crate::meat::AppId,
    replicas: u32,
    current_replicas: Option<u32>,
) -> Result<Config, (StatusCode, String)> {
    use crate::config::app::ScaleRefusal;
    let Some(spec) = desired.apps.get(app_id) else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("there's no app {app_id} to scale"),
        ));
    };
    if let Some(refusal) = spec.scale_refusal(replicas) {
        return Err(match refusal {
            ScaleRefusal::ToZero => (
                StatusCode::BAD_REQUEST,
                format!(
                    "an app runs at least 1 replica; `relish stop {}` scales it to zero and \
                     keeps its count for the next start",
                    app_id.name
                ),
            ),
            ScaleRefusal::Autoscaled => (
                StatusCode::CONFLICT,
                format!(
                    "{app_id} is autoscaled, so the autoscaler sets its count; change \
                     [autoscale] min and max in its file instead"
                ),
            ),
            ScaleRefusal::DaemonSet => (
                StatusCode::CONFLICT,
                format!("{app_id} runs one replica on every node, so it has no count to set"),
            ),
            ScaleRefusal::TooMany => (
                StatusCode::BAD_REQUEST,
                format!(
                    "{replicas} replicas is more than the {} an app may run",
                    crate::config::MAX_REPLICAS
                ),
            ),
        });
    }
    // The count in the spec, which a scale changes. A stopped app keeps
    // it, and a scale starts the app again with the new one, as an apply
    // does.
    let current = match spec.replicas {
        crate::config::Replicas::Fixed(count) => count,
        // `scale_refusal` refused a daemon set above.
        crate::config::Replicas::DaemonSet => 0,
    };
    if let Some(expected) = current_replicas
        && expected != current
    {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "{app_id} has {current} replicas in its spec now, not {expected}: someone \
                 changed it since you looked; look again, then scale"
            ),
        ));
    }
    crate::bun::audit::annotate(crate::bun::audit::AuditNote {
        app: Some(app_id.name.clone()),
        namespace: Some(app_id.namespace.clone()),
        details: [
            ("replicas_from".to_string(), current.to_string()),
            ("replicas_to".to_string(), replicas.to_string()),
        ]
        .into(),
    });
    let mut spec = spec.clone();
    spec.replicas = crate::config::Replicas::Fixed(replicas);
    spec.namespace = Some(app_id.namespace.clone());
    // Ordinals belong to the placement, not the spec; an apply refuses
    // them, and the leader numbers the replicas again (#398).
    spec.ordinals = None;
    let mut config = Config::default();
    config.app.insert(app_id.name.clone(), spec);
    Ok(config)
}

fn scale_refused(status: StatusCode, message: String) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

/// Whether `relish stop`, `relish delete` or `relish release` is changing
/// an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AppChange {
    /// Scale to zero, keeping the specification, until the next apply.
    Stop,
    /// Remove the app from desired state; `force` removes one GitOps
    /// manages.
    Delete { force: bool },
    /// Clear the app's owner (#1125).
    Release,
}

impl AppChange {
    pub(super) fn verb(self) -> &'static str {
        match self {
            AppChange::Stop => "stop",
            AppChange::Delete { .. } => "delete",
            AppChange::Release => "release",
        }
    }
}

/// The 409 for a delete or rollback of a GitOps-managed app without
/// `force`, or `None` when the change may go ahead (#693).
pub(super) async fn gitops_managed_refusal(
    council: &crate::council::CouncilNode,
    app: &str,
    namespace: &str,
    verb: &str,
    force: bool,
) -> Option<Response> {
    if force {
        return None;
    }
    let app_id = crate::meat::AppId::new(app, namespace);
    if !council
        .desired_state()
        .await
        .gitops_owned
        .apps
        .contains(&app_id)
    {
        return None;
    }
    Some(
        (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": crate::lettuce::types::managed_change_refusal(verb, app, namespace)
            })),
        )
            .into_response(),
    )
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
    auth: Option<&crate::sesame::auth::AuthContext>,
    caller_headers: &HeaderMap,
) -> Response {
    let app_id = crate::meat::AppId::new(&app, &namespace);
    // Followers can't write to Raft (openraft does not forward client
    // writes), so forward the whole request to the leader's API, as the
    // caller: the leader checks the owner below against its own desired
    // state, which a follower's replica can lag (#1442).
    if !crate::bun::leadership::leads(&council).await {
        let leader_url = match leader_api_url(&state, &council, caller_headers).await {
            Ok(url) => url,
            Err(refusal) => return refusal.into_response(),
        };
        let force = if matches!(change, AppChange::Delete { force: true }) {
            "?force=true"
        } else {
            ""
        };
        let path = match crate::cluster::http::peer_path(&[
            "v1",
            change.verb(),
            app.as_str(),
            namespace.as_str(),
        ]) {
            Ok(path) => path,
            Err(error) => return peer_path_refusal(&error),
        };
        let url = format!("{leader_url}{path}{force}");
        let request = copy_forwarded_auth(
            mark_leader_forwarded(state.cluster_http.client().post(url)),
            caller_headers,
        );
        return crate::bun::batch::relay_leader_answer(
            request,
            crate::bun::batch::LEADER_FORWARD_TIMEOUT,
        )
        .await;
    }

    // Only the owner's token or an admin changes an owned app (#1125),
    // checked here on the leader, which proposes the write.
    if let Some(refusal) =
        super::apply::owner_command_refusal(&council, auth, &app_id, change.verb()).await
    {
        return refusal;
    }
    if let AppChange::Delete { force } = change
        && let Some(refusal) =
            gitops_managed_refusal(&council, &app, &namespace, "delete", force).await
    {
        return refusal;
    }
    // Someone else's lease on the app stops a person's write, by name,
    // unless they asked to break it (#1686).
    let break_lease = match crate::bun::lease_fence::break_lease_requested(caller_headers) {
        Ok(requested) => requested,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": error })),
            )
                .into_response();
        }
    };
    let apps = [app_id.clone()];
    let request = match change {
        AppChange::Stop => crate::council::types::RaftRequest::AppStop { app_id },
        AppChange::Delete { .. } => crate::council::types::RaftRequest::AppDelete { app_id },
        AppChange::Release => crate::council::types::RaftRequest::AppRelease { app_id },
    };
    let mut written = None;
    for _ in 0..crate::bun::lease_fence::LEASE_ATTEMPTS {
        let generation =
            match super::leases::clear_leases_for_write(&state, &council, auth, &apps, break_lease)
                .await
            {
                Ok(generation) => generation,
                Err(refusal) => return refusal.into_response(),
            };
        let fenced = crate::bun::lease_fence::fence(generation, request.clone());
        match council.write(fenced).await {
            Ok(crate::council::CouncilResponse::LeaseGenerationMoved { .. }) => continue,
            other => {
                written = Some(other);
                break;
            }
        }
    }
    let Some(written) = written else {
        return super::leases::LeaseWriteRefusal::Churn.into_response();
    };
    match written {
        Ok(crate::council::CouncilResponse::Refused { reason }) => {
            let status = match change {
                AppChange::Release => StatusCode::CONFLICT,
                AppChange::Stop | AppChange::Delete { .. } => StatusCode::NOT_FOUND,
            };
            (status, Json(serde_json::json!({ "error": reason }))).into_response()
        }
        Ok(_) => {
            let status = match change {
                AppChange::Stop => "stopping",
                AppChange::Delete { .. } => "deleting",
                AppChange::Release => "released",
            };
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "status": status })),
            )
                .into_response()
        }
        // A council that can't commit in time is unavailable, not broken.
        Err(e) => {
            let status = match e {
                crate::council::CouncilError::WriteTimedOut { .. } => {
                    StatusCode::SERVICE_UNAVAILABLE
                }
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (
                status,
                Json(serde_json::json!({
                    "error": format!("failed to update desired state: {e}")
                })),
            )
                .into_response()
        }
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
    /// How long the command may run, in seconds. Absent means the
    /// agent's cap; a larger value is held to it.
    #[serde(default)]
    pub(super) timeout_seconds: Option<u64>,
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
        timeout: crate::bun::agent::exec_timeout(body.timeout_seconds),
        response,
    })
    .await
    {
        // A command that ran answers 200 whatever its exit code: the code is
        // the command's answer, not the API's (D34).
        Ok(Ok(output)) => Json(output).into_response(),
        Ok(Err(error)) => {
            let status = match error {
                crate::bun::BunError::AppNotFound { .. }
                | crate::bun::BunError::InstanceNotFound { .. }
                | crate::bun::BunError::Grill(crate::grill::GrillError::NotFound { .. }) => {
                    StatusCode::NOT_FOUND
                }
                crate::bun::BunError::ExecTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,
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
