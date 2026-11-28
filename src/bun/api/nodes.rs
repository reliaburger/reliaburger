//! Cluster node routes: the node list, the node relay, council status and
//! manual elections.

use super::*;
use crate::council::handover::HANDOVER_HOLD;

/// Marks the request a voter taking over leadership sends the current
/// leader first: hold your elections, so you can't win back the election my
/// campaign starts (#755).
pub(super) const ELECT_HOLD_HEADER: &str = "x-reliaburger-elect-hold";

/// How long the voter taking over keeps campaigning. Shorter than the
/// leader's [`HANDOVER_HOLD`], so the hold outlasts every campaign.
const TAKE_OVER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// How long the leader may take to accept a hold.
const HOLD_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Move leadership to this voter, or at least off the current leader
/// (admin; for planned maintenance on the leader's node).
///
/// openraft 0.9 has no leadership transfer, so this node first asks the
/// leader to hold its elections, then campaigns until another voter leads.
/// The winner is usually this node; a voter whose log is longer than this
/// node's can win instead, which still moves leadership off the old leader.
/// Answers with the new leader once one leads. With [`ELECT_HOLD_HEADER`]
/// set, this is that hold request: this node holds its elections and answers
/// at once.
pub(super) async fn cluster_elect_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    let Some(council) = &state.council else {
        return elect_error(StatusCode::SERVICE_UNAVAILABLE, "no council on this node");
    };
    if headers.contains_key(ELECT_HOLD_HEADER) {
        council.hold_elections_until(std::time::Instant::now() + HANDOVER_HOLD);
        return Json(serde_json::json!({ "status": "holding elections" })).into_response();
    }
    let (self_id, previous, is_voter) = {
        let metrics = council.metrics();
        let metrics = metrics.borrow();
        let is_voter = metrics
            .membership_config
            .membership()
            .voter_ids()
            .any(|id| id == metrics.id);
        (metrics.id, metrics.current_leader, is_voter)
    };
    if previous == Some(self_id) {
        return elect_error(
            StatusCode::CONFLICT,
            "this node already leads; send the request to the voter that should take over",
        );
    }
    if !is_voter {
        return elect_error(
            StatusCode::CONFLICT,
            "this node is not a council voter, so it can't lead",
        );
    }
    if previous.is_some()
        && let Err(response) =
            ask_leader_to_hold(&state, council, directory.as_deref(), &headers).await
    {
        return response;
    }
    let deadline = std::time::Instant::now() + TAKE_OVER_DEADLINE;
    match council
        .campaign_until_leadership_moves(previous, deadline)
        .await
    {
        Ok(leader) => Json(serde_json::json!({
            "status": "leadership moved",
            "leader": council_member_name(council, leader),
            "previous_leader": previous.map(|id| council_member_name(council, id)),
        }))
        .into_response(),
        Err(error @ crate::council::CouncilError::HandoverTimedOut { .. }) => {
            elect_error(StatusCode::GATEWAY_TIMEOUT, &error.to_string())
        }
        Err(error) => elect_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

fn elect_error(status: StatusCode, error: &str) -> Response {
    (status, Json(serde_json::json!({ "error": error }))).into_response()
}

/// A council member's node name, or its Raft id when the membership doesn't
/// name it.
fn council_member_name(council: &crate::council::CouncilNode, id: u64) -> String {
    let metrics = council.metrics();
    let metrics = metrics.borrow();
    metrics
        .membership_config
        .membership()
        .get_node(&id)
        .map(|node| node.name.clone())
        .unwrap_or_else(|| id.to_string())
}

/// Ask the current leader to hold its elections before this node campaigns.
///
/// The caller's own credential travels with the request, so the leader
/// repeats the cluster-admin check. Without the hold the old leader often
/// wins back the election this node's campaign starts, so a refusal ends
/// the request rather than campaigning anyway.
// `Response` is large but it IS the HTTP reply to send on failure.
#[allow(clippy::result_large_err)]
async fn ask_leader_to_hold(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    directory: Option<&LeaderDirectory>,
    headers: &HeaderMap,
) -> Result<(), Response> {
    let leader_url = super::upgrade::leader_base_url(state, council, directory, headers)
        .await
        .map_err(|refusal| elect_error(StatusCode::SERVICE_UNAVAILABLE, &refusal.to_string()))?;
    let request = mark_leader_forwarded(
        state
            .cluster_http
            .client()
            .post(format!("{leader_url}/v1/cluster/elect"))
            .header(ELECT_HOLD_HEADER, "1"),
    );
    let request = copy_forwarded_auth(request, headers);
    match tokio::time::timeout(HOLD_REQUEST_TIMEOUT, request.send()).await {
        Ok(Ok(response)) if response.status().is_success() => Ok(()),
        Ok(Ok(response)) => {
            let status = response.status();
            let detail = response.text().await.unwrap_or_default();
            Err(elect_error(
                StatusCode::BAD_GATEWAY,
                &format!("the leader refused to hold its elections ({status}): {detail}"),
            ))
        }
        Ok(Err(error)) => Err(elect_error(
            StatusCode::BAD_GATEWAY,
            &format!("failed to ask the leader to hold its elections: {error}"),
        )),
        Err(_) => Err(elect_error(
            StatusCode::GATEWAY_TIMEOUT,
            "the leader did not answer the hold request in time",
        )),
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
    match super::summary::cluster_members(&state, known.as_deref()).await {
        Ok(nodes) => Json(nodes).into_response(),
        Err(error) => unavailable_response(error),
    }
}

/// Largest request body the node relay forwards (a path request is tiny).
pub(super) const MAX_RELAY_REQUEST_BYTES: usize = 64 * 1024;
/// Largest response the node relay passes back (an events page is the biggest).
pub(super) const MAX_RELAY_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// A path probe runs for up to 25 seconds on the target; allow for the hop.
pub(super) const RELAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The per-node requests `relish wtf`, `relish inspect`, `relish path`,
/// `relish test` and `relish exec` make, and nothing else. The relay is a
/// reachability aid, not a general proxy.
///
/// `path` is the request's path after the relay prefix, percent-decoded.
/// Returns the path the target node will receive, built segment by segment
/// with [`crate::cluster::http::peer_path`], when the relay forwards it.
///
/// The allowlist is checked against that built path, not against `path`,
/// and only a path that needed no encoding passes. A URL resolves `.` and
/// `..`, drops tabs and newlines and, for `http`, reads `\` as `/`, so a
/// segment holding any of those, or `%`, `?`, `#` or a control character,
/// could reach a route the allowlist never named. Such a path is refused.
pub(super) fn relay_peer_path(method: &axum::http::Method, path: &str) -> Option<String> {
    let segments: Vec<&str> = path.split('/').collect();
    let peer_path = crate::cluster::http::peer_path(&segments).ok()?;
    let sent = peer_path.strip_prefix('/')?;
    if sent != path {
        return None;
    }
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
        // Each node knows whether its own `[gitops]` drifts (#1286).
        "v1/gitops",
        // `relish nodes` shows each node's version and usage.
        "v1/cluster/summary/node",
    ];
    let allowed = match *method {
        // `relish test` compares each node's own deploy history.
        axum::http::Method::GET => READS.contains(&sent) || is_deploy_history_path(sent),
        // `relish exec` reaches an instance on another node this way too;
        // the target repeats the exec authorisation with the caller's token.
        axum::http::Method::POST => sent == "v1/path" || is_exec_path(sent),
        _ => false,
    };
    allowed.then_some(peer_path)
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
/// never adds the node's service identity, and marks the request as
/// forwarded for a person, so a node-only route on the target refuses it.
pub(super) async fn node_relay_handler(
    State(state): State<ApiState>,
    known: Option<axum::Extension<KnownMembers>>,
    Path((node, path)): Path<(String, String)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(peer_path) = relay_peer_path(&method, &path) else {
        return (
            StatusCode::NOT_FOUND,
            format!(
                "the node relay does not forward {method} /{}",
                path.escape_debug()
            ),
        )
            .into_response();
    };
    if asks_to_follow_events(&peer_path, uri.query()) {
        return (
            StatusCode::BAD_REQUEST,
            "the node relay does not forward a followed event stream: ask the node \
             directly, or use /v1/events?follow=true, which already follows every node",
        )
            .into_response();
    }
    let mut url = match known_node_api_url(&state, known.as_deref(), &node, &peer_path).await {
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
    relay_exchange(request, &node, RELAY_TIMEOUT).await
}

/// Whether a relayed request asks for a followed event stream, which never
/// ends: `follow=true` on `v1/events`.
fn asks_to_follow_events(peer_path: &str, query: Option<&str>) -> bool {
    peer_path == "/v1/events"
        && query.is_some_and(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .any(|(key, value)| key == "follow" && value != "false")
        })
}

/// Send `request` to `node` and pass back its whole answer, or a 504 if the
/// answer, body included, isn't in within `timeout`. A peer that sends its
/// headers and then stalls used to hold the relay for as long as the caller
/// waited (#2393).
async fn relay_exchange(
    request: reqwest::RequestBuilder,
    node: &str,
    timeout: std::time::Duration,
) -> Response {
    match tokio::time::timeout(timeout, read_relayed_answer(request, node)).await {
        Ok(response) => response,
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "node {node} did not answer within {}s",
                timeout.as_secs_f64()
            ),
        )
            .into_response(),
    }
}

async fn read_relayed_answer(request: reqwest::RequestBuilder, node: &str) -> Response {
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("node {node} did not answer: {error}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_refuses_dot_segments() {
        let post = axum::http::Method::POST;
        let get = axum::http::Method::GET;
        assert_eq!(
            relay_peer_path(&post, "v1/exec/web/default").as_deref(),
            Some("/v1/exec/web/default")
        );
        assert_eq!(
            relay_peer_path(&get, "v1/deploys/history/web").as_deref(),
            Some("/v1/deploys/history/web")
        );
        for path in [
            "v1/exec/../apply",
            "v1/exec/web/..",
            "v1/exec/./default",
            "v1/exec/%2e%2e/apply",
            "v1/exec/%2E./apply",
        ] {
            assert_eq!(relay_peer_path(&post, path), None, "relayed POST /{path}");
        }
        for path in ["v1/deploys/history/..", "v1/deploys/history/%2e%2e"] {
            assert_eq!(relay_peer_path(&get, path), None, "relayed GET /{path}");
        }
    }

    /// `..%5C..%5Ctoken%5Clist` decodes to one segment the old
    /// allowlist read as an app name, but a URL reads `\` as `/` and then
    /// resolves the `..`s, so it would have left as `GET /v1/token/list`.
    #[test]
    fn relay_refuses_backslashes_in_a_segment() {
        let get = axum::http::Method::GET;
        let post = axum::http::Method::POST;
        for path in [
            "v1/deploys/history/..\\..\\token\\list",
            "v1/deploys/history/web\\x",
        ] {
            assert_eq!(relay_peer_path(&get, path), None, "relayed GET /{path}");
        }
        assert_eq!(
            relay_peer_path(&post, "v1/exec/..\\..\\apply/default"),
            None
        );
    }

    /// A URL parser drops tabs and newlines, so `.\t.` would arrive as `..`.
    #[test]
    fn relay_refuses_tab_newline_and_control_character_segments() {
        let get = axum::http::Method::GET;
        let post = axum::http::Method::POST;
        for path in [
            "v1/deploys/history/.\t.",
            "v1/deploys/history/.\n.",
            "v1/deploys/history/web\r",
            "v1/deploys/history/web\u{0}",
            "v1/status\t",
        ] {
            assert_eq!(relay_peer_path(&get, path), None, "relayed GET {path:?}");
        }
        assert_eq!(relay_peer_path(&post, "v1/exec/.\t./apply"), None);
    }

    /// The relay forwards only a path that reaches the peer exactly as
    /// written: one needing `%`-encoding is refused, not rewritten.
    #[test]
    fn relay_refuses_segments_a_url_would_encode() {
        let get = axum::http::Method::GET;
        for path in [
            "v1/deploys/history/web%2fx",
            "v1/deploys/history/web?x",
            "v1/deploys/history/web#x",
            "v1/deploys/history/web x",
        ] {
            assert_eq!(relay_peer_path(&get, path), None, "relayed GET /{path}");
        }
    }

    /// A peer that answers its headers and then sends nothing more used to
    /// hold the relay for as long as the caller waited: only `send()` had a
    /// deadline (#2393). The whole exchange has one now.
    #[tokio::test]
    async fn the_relay_gives_up_on_a_peer_that_stalls_mid_answer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      transfer-encoding: chunked\r\n\r\n5\r\nhello\r\n",
                )
                .await
                .unwrap();
            // Then nothing, for longer than any test waits.
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            drop(socket);
        });

        let request = reqwest::Client::new().get(format!("http://{address}/v1/events"));
        let started = std::time::Instant::now();
        let response =
            relay_exchange(request, "node-2", std::time::Duration::from_millis(300)).await;
        peer.abort();

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn only_a_followed_event_stream_is_refused() {
        assert!(asks_to_follow_events("/v1/events", Some("follow=true")));
        assert!(asks_to_follow_events(
            "/v1/events",
            Some("limit=5&follow=true")
        ));
        assert!(!asks_to_follow_events("/v1/events", Some("follow=false")));
        assert!(!asks_to_follow_events("/v1/events", None));
        assert!(!asks_to_follow_events("/v1/status", Some("follow=true")));
    }
}
