//! Volume snapshot routes: create, list, restore and delete.
//!
//! A managed volume lives on the node its app runs on, so every route first
//! finds that node in the desired state and forwards the request there under
//! the caller's own credential (#482). Another node may hold a stale copy
//! left by an earlier move, and must not answer from it.

use super::*;

/// Marks a snapshot request another node has already routed here, so it is
/// answered on this node and never forwarded again.
pub(super) const SNAPSHOT_FORWARDED_HEADER: &str = "x-reliaburger-snapshot-forwarded";

/// How long a forwarded snapshot request may take on the volume's node. A
/// Btrfs snapshot is quick, but sizing it walks the volume's files.
const SNAPSHOT_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Where a snapshot request for one app is answered.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SnapshotRoute {
    /// On this node: it holds the volume, or nothing says another does.
    Here,
    /// On the one other node that holds the volume.
    Forward(String),
    /// Nowhere: several other nodes each hold a replica's copy, so no one
    /// of them is the copy the caller meant.
    Ambiguous(Vec<String>),
}

/// Choose where to answer a snapshot request, given the nodes that hold the
/// app's managed volumes. An app with none on record (a standalone node, an
/// app the council doesn't know) is answered here, as the agent's own
/// inventory decides. A node that holds one of the copies answers for its
/// own replica.
pub(super) fn snapshot_route(self_name: &str, homes: &[String]) -> SnapshotRoute {
    match homes {
        [] => SnapshotRoute::Here,
        _ if homes.iter().any(|home| home == self_name) => SnapshotRoute::Here,
        [home] => SnapshotRoute::Forward(home.clone()),
        _ => SnapshotRoute::Ambiguous(homes.to_vec()),
    }
}

/// `None` when this node answers the request itself; otherwise the response
/// that settles it elsewhere: the volume node's own answer, or why it can't
/// be reached.
async fn route_or_answer(
    state: &ApiState,
    directory: Option<&LeaderDirectory>,
    request: ForwardedSnapshot<'_>,
) -> Option<Response> {
    if request.headers.contains_key(SNAPSHOT_FORWARDED_HEADER) {
        return None;
    }
    let source = super::node_info::DesiredAppsSource::Caller(directory);
    let apps = match super::node_info::gather_desired_apps(state, source).await {
        Ok(apps) => apps,
        Err(error) => {
            return Some(snapshot_refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "cannot tell which node holds the volumes of {}/{}: {error}",
                    request.namespace, request.app
                ),
            ));
        }
    };
    let homes = apps
        .into_iter()
        .find(|app| app.namespace == request.namespace && app.app == request.app)
        .map(|app| app.volume_homes)
        .unwrap_or_default();
    match snapshot_route(&local_node_name(state), &homes) {
        SnapshotRoute::Here => None,
        SnapshotRoute::Forward(node) => Some(forward_snapshot(state, &node, request).await),
        SnapshotRoute::Ambiguous(nodes) => Some(snapshot_refusal(
            StatusCode::CONFLICT,
            format!(
                "{}/{} keeps a volume on each of {}; send the request to one of those nodes",
                request.namespace,
                request.app,
                nodes.join(", ")
            ),
        )),
    }
}

fn snapshot_refusal(status: StatusCode, message: String) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

/// One snapshot request as it arrived, ready to send on to another node.
struct ForwardedSnapshot<'a> {
    namespace: &'a str,
    app: &'a str,
    method: axum::http::Method,
    uri: &'a axum::http::Uri,
    headers: &'a HeaderMap,
    /// The JSON body to send, for the routes that take one.
    body: Option<Vec<u8>>,
}

/// Send a snapshot request to the node that holds the volume, marked as
/// forwarded and carrying the caller's own credential, so that node repeats
/// every check, and pass its answer back.
async fn forward_snapshot(
    state: &ApiState,
    node: &str,
    request: ForwardedSnapshot<'_>,
) -> Response {
    let path = request
        .uri
        .path_and_query()
        .map_or(request.uri.path(), |path| path.as_str());
    let Ok(url) = target_node_api_url(state, node, path).await else {
        return snapshot_refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "the volumes of {}/{} live on {node}, which is not a live member; retry once it is back",
                request.namespace, request.app
            ),
        );
    };
    let mut forwarded = state
        .cluster_http
        .client()
        .request(request.method, url)
        .header(SNAPSHOT_FORWARDED_HEADER, "1");
    if let Some(body) = request.body {
        forwarded = forwarded
            .header(
                axum::http::header::CONTENT_TYPE.as_str(),
                "application/json",
            )
            .body(body);
    }
    let forwarded = copy_forwarded_auth(forwarded, request.headers);
    let exchange = async {
        let response = forwarded.send().await?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if body.len().saturating_add(chunk.len()) > super::nodes::MAX_RELAY_RESPONSE_BYTES {
                return Ok(None);
            }
            body.extend_from_slice(&chunk);
        }
        Ok::<_, reqwest::Error>(Some((status, body)))
    };
    match tokio::time::timeout(SNAPSHOT_FORWARD_TIMEOUT, exchange).await {
        Ok(Ok(Some((status, body)))) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            (
                status,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response()
        }
        Ok(Ok(None)) => snapshot_refusal(
            StatusCode::BAD_GATEWAY,
            format!("{node} answered the snapshot request with more than 8 MiB"),
        ),
        Ok(Err(error)) => snapshot_refusal(
            StatusCode::BAD_GATEWAY,
            format!("failed to forward the snapshot request to {node}: {error}"),
        ),
        Err(_) => snapshot_refusal(
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "{node} did not answer the snapshot request within {}s",
                SNAPSHOT_FORWARD_TIMEOUT.as_secs()
            ),
        ),
    }
}

#[derive(serde::Deserialize, serde::Serialize, Default)]
pub(super) struct SnapshotCreateBody {
    /// Container mount path; omitted = every provisioned volume.
    pub(super) volume: Option<String>,
    /// Custom snapshot name; omitted = unix-seconds timestamp.
    pub(super) name: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
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

// Each route takes the request's method, URI and headers whole, because a
// request for an app whose volume lives elsewhere goes on to that node.
#[allow(clippy::too_many_arguments)]
pub(super) async fn snapshot_create_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    Path((namespace, app)): Path<(String, String)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
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
    let request = ForwardedSnapshot {
        namespace: &namespace,
        app: &app,
        method,
        uri: &uri,
        headers: &headers,
        body: serde_json::to_vec(&body).ok(),
    };
    if let Some(response) = route_or_answer(&state, directory.as_deref(), request).await {
        return response;
    }
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
    directory: Option<axum::Extension<LeaderDirectory>>,
    Path((namespace, app)): Path<(String, String)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    let request = ForwardedSnapshot {
        namespace: &namespace,
        app: &app,
        method,
        uri: &uri,
        headers: &headers,
        body: None,
    };
    if let Some(response) = route_or_answer(&state, directory.as_deref(), request).await {
        return response;
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

#[allow(clippy::too_many_arguments)]
pub(super) async fn snapshot_restore_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    Path((namespace, app)): Path<(String, String)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
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
    let request = ForwardedSnapshot {
        namespace: &namespace,
        app: &app,
        method,
        uri: &uri,
        headers: &headers,
        body: serde_json::to_vec(&body).ok(),
    };
    if let Some(response) = route_or_answer(&state, directory.as_deref(), request).await {
        return response;
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

#[allow(clippy::too_many_arguments)]
pub(super) async fn snapshot_delete_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    Path((namespace, app, name)): Path<(String, String, String)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
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
    let request = ForwardedSnapshot {
        namespace: &namespace,
        app: &app,
        method,
        uri: &uri,
        headers: &headers,
        body: None,
    };
    if let Some(response) = route_or_answer(&state, directory.as_deref(), request).await {
        return response;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn homes(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn an_app_with_no_volume_home_on_record_is_answered_here() {
        assert_eq!(snapshot_route("node-1", &[]), SnapshotRoute::Here);
    }

    #[test]
    fn the_node_holding_the_volume_answers_for_it() {
        assert_eq!(
            snapshot_route("node-2", &homes(&["node-2"])),
            SnapshotRoute::Here
        );
        assert_eq!(
            snapshot_route("node-2", &homes(&["node-1", "node-2"])),
            SnapshotRoute::Here
        );
    }

    #[test]
    fn another_node_forwards_to_the_one_volume_home() {
        assert_eq!(
            snapshot_route("node-1", &homes(&["node-3"])),
            SnapshotRoute::Forward("node-3".to_string())
        );
    }

    #[test]
    fn another_node_refuses_when_several_nodes_hold_a_copy() {
        assert_eq!(
            snapshot_route("node-3", &homes(&["node-1", "node-2"])),
            SnapshotRoute::Ambiguous(homes(&["node-1", "node-2"]))
        );
    }
}
