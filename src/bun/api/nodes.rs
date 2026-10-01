//! Cluster node routes: the node list, the node relay, council status and
//! manual elections.

use super::*;

/// Ask this node to call a Raft election on itself (admin; manual
/// recovery tool — e.g. to move leadership off a node before maintenance).
pub(super) async fn cluster_elect_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council on this node" })),
        )
            .into_response();
    };
    match council.raft().trigger().elect().await {
        Ok(()) => Json(serde_json::json!({ "status": "election triggered" })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// List cluster nodes: gossip's live members, then the ones this node
/// remembers as dead.
///
/// Gossip's live view drops a member the moment it is declared dead, and the
/// scheduler, council and Pickle rely on that. A listing is for people,
/// though, and a node that vanished is harder to act on than one marked
/// dead, so down members come from [`KnownMembers`] instead.
pub(super) async fn nodes_handler(
    State(state): State<ApiState>,
    known: Option<axum::Extension<KnownMembers>>,
) -> Response {
    let down = match known {
        Some(known) => known
            .down()
            .await
            .into_iter()
            .map(|member| crate::bun::agent::NodeStatus {
                node_id: member.info.node_id.0.clone(),
                address: member.gossip_address.to_string(),
                api_address: member.info.api_advertised.then_some(member.info.address),
                state: member.state.to_string(),
                incarnation: member.incarnation,
                is_council: false,
                is_leader: false,
                labels: member.labels,
            })
            .collect(),
        None => Vec::new(),
    };
    match ask_agent(&state.cmd_tx, |response| AgentCommand::Nodes {
        down,
        response,
    })
    .await
    {
        Ok(mut nodes) => {
            if let Some(membership) = &state.membership {
                let members = membership.read().await;
                // A down row already carries its last advertised address,
                // and the live table has none for it.
                for node in nodes.iter_mut().filter(|n| n.api_address.is_none()) {
                    node.api_address = members
                        .iter()
                        .find(|member| member.node_id.0 == node.node_id && member.api_advertised)
                        .map(|member| member.address);
                }
            }
            Json(nodes).into_response()
        }
        Err(response) => response,
    }
}

/// Largest request body the node relay forwards (a path request is tiny).
pub(super) const MAX_RELAY_REQUEST_BYTES: usize = 64 * 1024;
/// Largest response the node relay passes back (an events page is the biggest).
pub(super) const MAX_RELAY_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// A path probe runs for up to 25 seconds on the target; allow for the hop.
pub(super) const RELAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The per-node reads `relish wtf`, `relish inspect`, `relish path` and
/// `relish test` make, and nothing else. The relay is a reachability aid, not a general proxy.
pub(super) fn relay_allows(method: &axum::http::Method, path: &str) -> bool {
    const READS: &[&str] = &[
        "v1/health",
        "v1/status",
        "v1/diagnostics",
        "v1/diagnostics/apps",
        "v1/events",
        "v1/deploys/operations",
        "v1/alerts",
        "v1/fault",
        "v1/cluster/council",
        "v1/cluster/nodes",
        "v1/capabilities",
        "v1/version",
    ];
    match *method {
        // `relish test` compares each node's own deploy history.
        axum::http::Method::GET => READS.contains(&path) || is_deploy_history_path(path),
        // `relish exec` reaches an instance on another node this way too;
        // the target repeats the exec authorisation with the caller's token.
        axum::http::Method::POST => path == "v1/path" || is_exec_path(path),
        _ => false,
    }
}

/// `v1/deploys/history/{app}` and nothing longer (the namespace is a query).
pub(super) fn is_deploy_history_path(path: &str) -> bool {
    path.strip_prefix("v1/deploys/history/")
        .is_some_and(|app| !app.is_empty() && !app.contains('/'))
}

/// `v1/exec/{app}/{namespace}` and nothing longer.
pub(super) fn is_exec_path(path: &str) -> bool {
    let mut segments = path.split('/');
    segments.next() == Some("v1")
        && segments.next() == Some("exec")
        && segments.next().is_some_and(|app| !app.is_empty())
        && segments
            .next()
            .is_some_and(|namespace| !namespace.is_empty())
        && segments.next().is_none()
}

/// `GET|POST /v1/nodes/{node}/relay/{path}`: send one of a few per-node
/// diagnostic requests to a named node and return its answer.
///
/// A laptop host can reach node 1's forwarded port but not the guests' own
/// addresses, so `relish wtf` and `relish path` reach every other node
/// through this. The caller's own credential travels with the request and the
/// target repeats every authentication and authorisation check; the relay
/// never adds the node's service identity.
pub(super) async fn node_relay_handler(
    State(state): State<ApiState>,
    known: Option<axum::Extension<KnownMembers>>,
    Path((node, path)): Path<(String, String)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !relay_allows(&method, &path) {
        return (
            StatusCode::NOT_FOUND,
            format!("the node relay does not forward {method} /{path}"),
        )
            .into_response();
    }
    let mut url =
        match known_node_api_url(&state, known.as_deref(), &node, &format!("/{path}")).await {
            Ok(url) => url,
            Err(response) => return response,
        };
    if let Some(query) = uri.query() {
        url.push('?');
        url.push_str(query);
    }
    let mut request = state.cluster_http.client().request(method.clone(), url);
    if method == axum::http::Method::POST {
        request = request
            .header(
                axum::http::header::CONTENT_TYPE.as_str(),
                "application/json",
            )
            .body(body);
    }
    let request = copy_forwarded_auth(request, &headers);
    let response = match tokio::time::timeout(RELAY_TIMEOUT, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("node {node} did not answer: {error}"),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "node {node} did not answer within {}s",
                    RELAY_TIMEOUT.as_secs()
                ),
            )
                .into_response();
        }
    };
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return (
                StatusCode::BAD_GATEWAY,
                format!("node {node} broke off its answer"),
            )
                .into_response();
        };
        if bytes.len() + chunk.len() > MAX_RELAY_RESPONSE_BYTES {
            return (
                StatusCode::BAD_GATEWAY,
                format!("node {node} answered with more than the relay's 8 MiB limit"),
            )
                .into_response();
        }
        bytes.extend_from_slice(&chunk);
    }
    let mut relayed = (status, bytes).into_response();
    if let Some(content_type) = content_type
        && let Ok(value) = axum::http::HeaderValue::from_str(&content_type)
    {
        relayed
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, value);
    }
    relayed
}

/// Show council (Raft) status.
pub(super) async fn council_handler(State(state): State<ApiState>) -> Response {
    match ask_agent(&state.cmd_tx, |response| AgentCommand::Council { response }).await {
        Ok(council) => Json(serde_json::json!(council)).into_response(),
        Err(response) => response,
    }
}
