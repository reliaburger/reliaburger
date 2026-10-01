//! Volume snapshot routes: create, list, restore and delete.

use super::*;

#[derive(serde::Deserialize, Default)]
pub(super) struct SnapshotCreateBody {
    /// Container mount path; omitted = every provisioned volume.
    pub(super) volume: Option<String>,
    /// Custom snapshot name; omitted = unix-seconds timestamp.
    pub(super) name: Option<String>,
}

#[derive(serde::Deserialize)]
pub(super) struct SnapshotRestoreBody {
    pub(super) name: String,
    /// Container mount path; required when several volumes share the name.
    pub(super) volume: Option<String>,
}

#[derive(serde::Deserialize)]
pub(super) struct SnapshotDeleteQuery {
    /// Container mount path; required when several volumes share the name.
    pub(super) volume: Option<String>,
}

/// Map snapshot failures to honest status codes: a running app, an
/// ambiguous name or volumes another operation owns is a conflict, missing
/// things are 404, a non-btrfs volume or an out-of-scope input is the
/// client's problem, anything else is ours.
pub(super) fn snapshot_error_response(error: &crate::bun::BunError) -> Response {
    use crate::grill::snapshot::SnapshotError;
    let status = match error {
        crate::bun::BunError::Snapshot(
            SnapshotError::AppRunning { .. }
            | SnapshotError::Ambiguous { .. }
            | SnapshotError::Busy { .. }
            | SnapshotError::RestoreInProgress { .. },
        ) => StatusCode::CONFLICT,
        crate::bun::BunError::Snapshot(
            SnapshotError::NotFound { .. } | SnapshotError::NoVolumes { .. },
        ) => StatusCode::NOT_FOUND,
        crate::bun::BunError::Snapshot(
            SnapshotError::UnsupportedFilesystem { .. }
            | SnapshotError::TestStorage
            | SnapshotError::InvalidInput(_),
        ) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}

pub(super) async fn snapshot_create_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((namespace, app)): Path<(String, String)>,
    body: Option<Json<SnapshotCreateBody>>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    let Json(body) = body.unwrap_or_default();
    match ask_agent(&state.cmd_tx, |response| AgentCommand::SnapshotCreate {
        namespace,
        app_name: app,
        volume: body.volume,
        name: body.name,
        response,
    })
    .await
    {
        Ok(Ok(metas)) => (StatusCode::CREATED, Json(serde_json::json!(metas))).into_response(),
        Ok(Err(e)) => snapshot_error_response(&e),
        Err(response) => response,
    }
}

pub(super) async fn snapshot_list_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((namespace, app)): Path<(String, String)>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::SnapshotList {
        namespace,
        app_name: app,
        response,
    })
    .await
    {
        Ok(Ok(metas)) => Json(serde_json::json!(metas)).into_response(),
        Ok(Err(e)) => snapshot_error_response(&e),
        Err(response) => response,
    }
}

pub(super) async fn snapshot_restore_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((namespace, app)): Path<(String, String)>,
    Json(body): Json<SnapshotRestoreBody>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::SnapshotRestore {
        namespace,
        app_name: app,
        name: body.name,
        volume: body.volume,
        response,
    })
    .await
    {
        Ok(Ok(())) => Json(serde_json::json!({ "restored": true })).into_response(),
        Ok(Err(e)) => snapshot_error_response(&e),
        Err(response) => response,
    }
}

pub(super) async fn snapshot_delete_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((namespace, app, name)): Path<(String, String, String)>,
    Query(query): Query<SnapshotDeleteQuery>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::SnapshotDelete {
        namespace,
        app_name: app,
        name,
        volume: query.volume,
        response,
    })
    .await
    {
        Ok(Ok(())) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Ok(Err(e)) => snapshot_error_response(&e),
        Err(response) => response,
    }
}
