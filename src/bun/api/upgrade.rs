//! Self-upgrade routes: the local binary swap and the cluster-wide rolling
//! upgrade.

use super::*;

/// Apply a node-level upgrade directive (admin). Responds 202 once the
/// binary is verified and staged; the process execs moments later.
pub(super) async fn upgrade_apply_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    connection: Option<axum::Extension<crate::sesame::connection::ConnectionClosed>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    let directive: crate::upgrade::types::UpgradeDirective = match serde_json::from_str(&body) {
        Ok(directive) => directive,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid directive: {e}") })),
            )
                .into_response();
        }
    };

    let answer_delivered = connection.map(|axum::Extension(closed)| closed);
    match ask_agent(&state.cmd_tx, |response| AgentCommand::UpgradeApply {
        directive,
        response,
        answer_delivered,
    })
    .await
    {
        Ok(Ok(())) => exec_follows(serde_json::json!({ "status": "upgrading" })),
        Ok(Err(crate::bun::BunError::Upgrade(
            error @ crate::upgrade::UpgradeError::AlreadyRunning { .. },
        ))) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "already_running",
                "detail": error.to_string(),
            })),
        )
            .into_response(),
        // "Not right now" (the binary's registry is unreachable or
        // restarting) is a 503, so the orchestrator re-sends the directive
        // instead of pausing the whole run on one blip.
        Ok(Err(crate::bun::BunError::Upgrade(error))) if error.is_transient() => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => agent_unavailable(),
    }
}

/// A 202 the node execs straight after. `Connection: close` makes Hyper
/// close the connection once this answer is written, which is what the
/// agent waits for before it execs (see `ConnectionClosed`).
fn exec_follows(body: serde_json::Value) -> Response {
    (
        StatusCode::ACCEPTED,
        [(axum::http::header::CONNECTION, "close")],
        Json(body),
    )
        .into_response()
}

/// Node-level upgrade status: running version, in-flight marker, history.
pub(super) async fn upgrade_status_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return resp;
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::UpgradeStatus {
        response,
    })
    .await
    {
        Ok(Ok(status)) => Json(status).into_response(),
        Ok(Err(e)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => agent_unavailable(),
    }
}

/// Revert this node to a previous binary version (admin).
pub(super) async fn upgrade_rollback_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    connection: Option<axum::Extension<crate::sesame::connection::ConnectionClosed>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    #[derive(serde::Deserialize, Default)]
    struct RollbackRequest {
        #[serde(default)]
        version: Option<crate::upgrade::BinaryVersion>,
    }
    let request: RollbackRequest = if body.trim().is_empty() {
        RollbackRequest::default()
    } else {
        match serde_json::from_str(&body) {
            Ok(request) => request,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": format!("invalid request: {e}") })),
                )
                    .into_response();
            }
        }
    };

    let answer_delivered = connection.map(|axum::Extension(closed)| closed);
    match ask_agent(&state.cmd_tx, |response| AgentCommand::UpgradeRollback {
        version: request.version,
        response,
        answer_delivered,
    })
    .await
    {
        Ok(Ok(())) => exec_follows(serde_json::json!({ "status": "rolling back" })),
        Ok(Err(e)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => agent_unavailable(),
    }
}

/// Marks an upgrade control call a follower has already forwarded, so two
/// nodes that disagree about the leader can't pass it back and forth.
pub(super) const UPGRADE_FORWARDED_HEADER: &str = "x-reliaburger-upgrade-forwarded";

/// How long a follower waits for the leader to answer a forwarded upgrade
/// call. A start probes every node (five seconds each, concurrently) before
/// its Raft write, so the budget is well above that.
pub(super) const UPGRADE_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Send an upgrade control call on to the leader when this node isn't it.
///
/// Only the leader can record a run, and openraft doesn't forward client
/// writes, so `relish upgrade start` against a follower used to fail with
/// "not leader". `None` means handle the call here: this node leads (and if
/// it has just lost that, its Raft write says so). The caller's own
/// credential travels with the request, so the leader repeats every
/// authorisation check; the follower never adds its service identity.
pub(super) async fn forward_upgrade_to_leader(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    directory: Option<&LeaderDirectory>,
    path: &str,
    headers: &HeaderMap,
    body: &str,
) -> Option<Response> {
    let leads = {
        let metrics = council.metrics();
        let metrics = metrics.borrow();
        metrics.current_leader == Some(metrics.id)
    };
    if leads {
        return None;
    }
    let unavailable = |error: &str| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": error })),
        )
            .into_response()
    };
    if headers.contains_key(UPGRADE_FORWARDED_HEADER) {
        return Some(unavailable(
            "this node was named the leader but isn't; retry once the election settles",
        ));
    }
    let advertised = directory.and_then(|LeaderDirectory(directory)| {
        let metrics = council.metrics();
        let metrics = metrics.borrow();
        crate::cluster::directory::leader_api_address(&metrics, &directory.borrow())
    });
    let leader_url = match advertised {
        Some(address) => state.cluster_http.url(&address.to_string(), ""),
        None => match leader_api_url(state, council).await {
            Some(url) => url,
            None => return Some(unavailable("no cluster leader known yet; retry shortly")),
        },
    };
    let request = state
        .cluster_http
        .client()
        .post(format!("{leader_url}{path}"))
        .header(UPGRADE_FORWARDED_HEADER, "1")
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(body.to_string());
    let request = copy_forwarded_auth(request, headers);
    let exchange = async {
        let response = request.send().await?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .cloned();
        let body = response.bytes().await?;
        Ok::<_, reqwest::Error>((status, content_type, body))
    };
    let response = match tokio::time::timeout(UPGRADE_FORWARD_TIMEOUT, exchange).await {
        Ok(Ok((status, content_type, body))) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut response = (status, body).into_response();
            if let Some(content_type) = content_type
                && let Ok(value) = axum::http::HeaderValue::from_bytes(content_type.as_bytes())
            {
                response
                    .headers_mut()
                    .insert(axum::http::header::CONTENT_TYPE, value);
            }
            response
        }
        Ok(Err(error)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": format!("failed to forward the upgrade call to the leader: {error}")
            })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({ "error": "the leader did not answer the upgrade call in time" })),
        )
            .into_response(),
    };
    Some(response)
}

/// A node in a cluster upgrade start request.
#[derive(serde::Deserialize)]
pub(super) struct StartUpgradeNode {
    pub(super) node_id: String,
    /// The node's bun API address (`host:port`).
    pub(super) address: String,
    pub(super) role: crate::upgrade::types::NodeRole,
}

/// Refuse a run a two-voter council would hold in the council phase for
/// good: the orchestrator never takes a voter down without quorum to spare,
/// and a holding run isn't paused, so it couldn't be aborted either.
// `Response` is large but it IS the HTTP reply to send on failure.
#[allow(clippy::result_large_err)]
pub(super) fn check_council_can_roll(
    council: &crate::council::CouncilNode,
) -> Result<(), Response> {
    let configured_voters = council
        .metrics()
        .borrow()
        .membership_config
        .membership()
        .voter_ids()
        .count();
    crate::upgrade::plan::check_council_can_roll(configured_voters).map_err(|e| {
        (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response()
    })
}

/// Start a cluster-wide rolling upgrade (admin, leader only).
///
/// The caller (relish) has already pushed the binary blob to the leader's
/// Pickle registry; this handler records the plan in Raft and the
/// orchestrator loop takes it from there.
pub(super) async fn upgrade_start_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    #[derive(serde::Deserialize)]
    struct StartRequest {
        target_version: crate::upgrade::BinaryVersion,
        /// One build per platform the nodes run on.
        binaries: Vec<crate::upgrade::types::PlatformBinary>,
        #[serde(default = "default_parallel")]
        parallel: u32,
        /// Registry the nodes fetch the binary from (the leader's Pickle).
        registry_address: String,
        nodes: Vec<StartUpgradeNode>,
        #[serde(default)]
        direction: Option<crate::upgrade::types::UpgradeDirection>,
        /// Allow a target older than what the nodes run.
        #[serde(default)]
        allow_downgrade: bool,
    }
    fn default_parallel() -> u32 {
        1
    }

    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "cluster upgrades need a council (cluster mode)" })),
        )
            .into_response();
    };
    if let Some(forwarded) = forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        "/v1/upgrade/start",
        &headers,
        &body,
    )
    .await
    {
        return forwarded;
    }
    let request: StartRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid request: {e}") })),
            )
                .into_response();
        }
    };
    if request.nodes.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "nodes list must not be empty" })),
        )
            .into_response();
    }
    if request.binaries.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "binaries list must not be empty" })),
        )
            .into_response();
    }

    // Derive each node's authoritative role + address server-side from
    // gossip membership and the Raft voter set, then validate the client's
    // claims against it (UPG2). A caller cannot upgrade a node under a
    // false identity: an unknown node, a spoofed role or a mismatched
    // address is rejected here rather than trusted into the plan.
    let authoritative = match build_authoritative_view(&state, council).await {
        Ok(view) => view,
        Err(resp) => return resp,
    };
    let requested: Vec<crate::upgrade::plan::RequestedNode> = request
        .nodes
        .iter()
        .map(|node| crate::upgrade::plan::RequestedNode {
            node_id: node.node_id.clone(),
            address: node.address.clone(),
            role: node.role,
        })
        .collect();
    let mut derived_nodes = match crate::upgrade::plan::derive_upgrade_nodes(&requested, |id| {
        authoritative.get(id).cloned()
    }) {
        Ok(nodes) => nodes,
        Err(e) => return plan_error_response(&e),
    };

    if let Some(active) = council.desired_state().await.active_upgrade {
        return upgrade_in_progress(&active);
    }

    // Refuse same-version and unrequested downgrades before anything is
    // recorded: once in Raft, a same-version run would "complete" without
    // swapping a single byte.
    let (running, readiness) = probe_running_binaries(&state, &derived_nodes).await;
    // Record each node's platform now, so the orchestrator hands it the
    // build for its own architecture. A node that didn't answer gets its
    // platform from the orchestrator's first poll.
    for record in &mut derived_nodes {
        let name = format!("node {}", record.node_id);
        record.platform = running
            .iter()
            .find(|node| node.node == name)
            .and_then(|node| node.platform.clone());
    }
    let direction = request
        .direction
        .unwrap_or(crate::upgrade::types::UpgradeDirection::Upgrade);
    // Every node fetches an upgrade from Pickle and so demands the external
    // signature. A run the nodes will refuse would only pause and then block
    // every later start, so refuse it here instead.
    if direction == crate::upgrade::types::UpgradeDirection::Upgrade
        && let Err(e) = request.binaries.iter().try_for_each(|binary| {
            crate::upgrade::plan::check_network_prerequisites(
                binary.external_signature.as_deref(),
                &readiness,
            )
        })
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    match crate::upgrade::plan::check_platform_targets(
        &request.target_version,
        &request.binaries,
        request.allow_downgrade,
        &running,
    ) {
        Ok(crate::upgrade::plan::TargetCheck::Proceed) => {}
        Ok(crate::upgrade::plan::TargetCheck::AlreadyRunning) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "already_running",
                    "detail": format!(
                        "every node already runs {} with this exact binary; nothing to do",
                        request.target_version
                    ),
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    if let Err(resp) = check_council_can_roll(council) {
        return resp;
    }

    let upgrade_id = format!(
        "up-{}-{}",
        request.target_version,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default()
    );
    let upgrade = crate::upgrade::types::ClusterUpgradeState {
        upgrade_id: upgrade_id.clone(),
        target_version: request.target_version,
        binaries: request.binaries,
        parallel: request.parallel.max(1),
        direction,
        phase: crate::upgrade::types::ClusterUpgradePhase::Preparing,
        registry_address: request.registry_address,
        allow_downgrade: request.allow_downgrade,
        nodes: derived_nodes,
    };
    // Check the directive this node's platform gets here, before anything
    // is recorded: a candidate with other formats would otherwise pause the
    // run on the first node it reached. Another platform's build can't run
    // here; each of those nodes checks its own when directed.
    if let Ok(directive) = crate::upgrade::orchestrator::directive_for(
        &upgrade,
        Some(&crate::upgrade::metadata::platform_key()),
    ) && let Err(resp) = check_candidate_on_leader(&state, &directive).await
    {
        return resp;
    }

    match council
        .write(crate::council::types::RaftRequest::UpgradeUpdate {
            state: Box::new(upgrade),
        })
        .await
    {
        Ok(_) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "starting", "upgrade_id": upgrade_id })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("could not record the upgrade: {e}")
            })),
        )
            .into_response(),
    }
}

/// Ask every planned node what it runs and whether it can verify a
/// network upgrade, for the start-time gates.
pub(super) async fn probe_running_binaries(
    state: &ApiState,
    nodes: &[crate::upgrade::types::NodeUpgradeRecord],
) -> (
    Vec<crate::upgrade::plan::RunningBinary>,
    Vec<crate::upgrade::plan::NetworkReadiness>,
) {
    probe_planned_nodes(state, nodes)
        .await
        .into_iter()
        .map(|(node, probe)| {
            (
                crate::upgrade::plan::RunningBinary {
                    node: node.clone(),
                    version: probe.version,
                    sha256: probe.binary_sha256,
                    platform: probe.platform,
                },
                crate::upgrade::plan::NetworkReadiness {
                    node,
                    accepts_network_upgrades: probe.accepts_network_upgrades,
                },
            )
        })
        .unzip()
}

/// Probe every planned node, named as an error names it (`node n1`).
///
/// Probes run concurrently, each bounded. An unreachable node is left out:
/// the orchestrator re-checks every node as the walk reaches it.
pub(super) async fn probe_planned_nodes(
    state: &ApiState,
    nodes: &[crate::upgrade::types::NodeUpgradeRecord],
) -> Vec<(String, crate::upgrade::orchestrator::NodeProbe)> {
    use crate::upgrade::orchestrator::NodeControl as _;

    let control = crate::upgrade::orchestrator::HttpNodeControl::with_http(
        state.service_token.clone(),
        state.cluster_http.clone(),
    );
    let probes = nodes.iter().map(|record| {
        let control = &control;
        async move {
            let probe = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                control.probe(&record.address),
            )
            .await
            .ok()
            .flatten()?;
            Some((format!("node {}", record.node_id), probe))
        }
    });
    futures_util::future::join_all(probes)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// How long the leader spends fetching, verifying and querying a
/// candidate before recording a run. It stays under the time a follower
/// waits for a forwarded start.
pub(super) const CANDIDATE_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Ask the candidate for its formats on the leader, the way every node
/// will, so a release the cluster can't run is refused before it's
/// recorded rather than paused on the first node (#339).
///
/// Only a definite answer refuses: other formats, a bad signature, a blob
/// the registry doesn't hold. A registry that's down right now, or a check
/// that runs out of time, proves nothing about the candidate, so the run is
/// recorded and the nodes check it themselves, riding out the outage as
/// they always have. A leader without an upgrade manager can't check
/// either; the nodes still do.
// `Response` is large but it IS the HTTP reply to send on failure.
#[allow(clippy::result_large_err)]
pub(super) async fn check_candidate_on_leader(
    state: &ApiState,
    directive: &crate::upgrade::types::UpgradeDirective,
) -> Result<(), Response> {
    let Some(manager) = &state.upgrade else {
        return Ok(());
    };
    let unchecked = |reason: String| {
        eprintln!(
            "bun: could not check the candidate {} before recording the upgrade ({reason}); \
             each node checks it when directed",
            directive.target_version
        );
        Ok(())
    };
    match tokio::time::timeout(CANDIDATE_CHECK_TIMEOUT, manager.check_candidate(directive)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) if e.is_transient() => unchecked(e.to_string()),
        Ok(Err(e)) => Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("refusing to upgrade to {}: {e}", directive.target_version)
            })),
        )
            .into_response()),
        Err(_) => unchecked(format!(
            "no answer within {}s",
            CANDIDATE_CHECK_TIMEOUT.as_secs()
        )),
    }
}

/// The 409 for a start or rollback while another run is active. A paused
/// run says how to get out of it: resume, abort or roll back.
pub(super) fn upgrade_in_progress(active: &crate::upgrade::types::ClusterUpgradeState) -> Response {
    let error = match &active.phase {
        crate::upgrade::types::ClusterUpgradePhase::Paused { reason } => format!(
            "upgrade {} is paused ({reason}); run `relish upgrade resume`, \
             `relish upgrade abort`, or `relish upgrade rollback <version>` first",
            active.upgrade_id
        ),
        _ => format!("upgrade {} is already in progress", active.upgrade_id),
    };
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({ "error": error })),
    )
        .into_response()
}

/// Archive a paused run that the operator ended: record it as `Aborted`,
/// then move it to history.
///
/// Two Raft writes. If the second is lost, the orchestrator archives the
/// aborted run on its next tick, and a start meanwhile gets a 409 that
/// names it.
// `Response` is large but it IS the HTTP reply to send on failure.
#[allow(clippy::result_large_err)]
pub(super) async fn archive_aborted_upgrade(
    council: &crate::council::CouncilNode,
    aborted: crate::upgrade::types::ClusterUpgradeState,
) -> Result<(), Response> {
    let upgrade_id = aborted.upgrade_id.clone();
    let writes = [
        crate::council::types::RaftRequest::UpgradeUpdate {
            state: Box::new(aborted),
        },
        crate::council::types::RaftRequest::UpgradeClear { upgrade_id },
    ];
    for write in writes {
        if let Err(e) = council.write(write).await {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("could not end the paused upgrade: {e}")
                })),
            )
                .into_response());
        }
    }
    Ok(())
}

/// Reply to a refused upgrade plan. A node whose endpoint the leader hasn't
/// heard yet is a 503 (retry shortly); a claim that contradicts the cluster
/// is the caller's fault, a 400.
pub(super) fn plan_error_response(error: &crate::upgrade::plan::PlanError) -> Response {
    let status = if error.is_transient() {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::BAD_REQUEST
    };
    (
        status,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}

/// Build the leader's authoritative view of every node for upgrade
/// planning (UPG2): node id → its API address (from gossip membership) and
/// role (from the Raft voter set + current leader). This is the source of
/// truth the client's start request is validated against.
///
/// The role comes from Raft: the current leader is `Leader`, other voters
/// are `Council`, and everything else `Worker`. Gossip identifies nodes by
/// name; the Raft voter set by `raft_id_from_name(name)`, so we bridge them
/// with that same stable hash.
// `Response` is large but it IS the HTTP reply to send on failure —
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
pub(super) async fn build_authoritative_view(
    state: &ApiState,
    council: &Arc<crate::council::CouncilNode>,
) -> Result<std::collections::HashMap<String, crate::upgrade::plan::AuthoritativeNode>, Response> {
    use crate::cluster::identity::raft_id_from_name;
    use crate::upgrade::plan::AuthoritativeNode;

    let Some(membership) = &state.membership else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no gossip membership on this node" })),
        )
            .into_response());
    };

    let metrics = council.metrics().borrow().clone();
    let voters: std::collections::BTreeSet<u64> =
        metrics.membership_config.membership().voter_ids().collect();
    let leader_id = metrics.current_leader;

    let mut view = std::collections::HashMap::new();
    for member in membership.read().await.iter() {
        let name = member.node_id.0.clone();
        let raft_id = raft_id_from_name(&name);
        let role = crate::upgrade::plan::role_from_raft(raft_id, leader_id, &voters);
        view.insert(
            name,
            AuthoritativeNode {
                address: member.api_advertised.then(|| member.address.to_string()),
                role,
            },
        );
    }
    Ok(view)
}

/// Cluster upgrade state, readable from any node (it's replicated).
pub(super) async fn upgrade_cluster_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return resp;
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": crate::upgrade::NO_COUNCIL })),
        )
            .into_response();
    };
    let desired = council.desired_state().await;
    Json(serde_json::json!({
        "active": desired.active_upgrade,
        "history": desired.upgrade_history,
    }))
    .into_response()
}

/// Un-pause a paused cluster upgrade (admin, leader only).
pub(super) async fn upgrade_resume_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": crate::upgrade::NO_COUNCIL })),
        )
            .into_response();
    };
    if let Some(forwarded) = forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        "/v1/upgrade/resume",
        &headers,
        "",
    )
    .await
    {
        return forwarded;
    }
    let Some(upgrade) = council.desired_state().await.active_upgrade else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no upgrade in progress" })),
        )
            .into_response();
    };
    if !matches!(
        upgrade.phase,
        crate::upgrade::types::ClusterUpgradePhase::Paused { .. }
    ) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "the upgrade is not paused" })),
        )
            .into_response();
    }

    let resumed = crate::upgrade::orchestrator::resume(upgrade);
    match council
        .write(crate::council::types::RaftRequest::UpgradeUpdate {
            state: Box::new(resumed),
        })
        .await
    {
        Ok(_) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "resumed" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// End a paused cluster upgrade in which no node moved (admin, leader
/// only). A run that already swapped nodes is refused with a pointer to
/// `relish upgrade rollback`, which walks them back.
pub(super) async fn upgrade_abort_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": crate::upgrade::NO_COUNCIL })),
        )
            .into_response();
    };
    if let Some(forwarded) = forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        "/v1/upgrade/abort",
        &headers,
        "",
    )
    .await
    {
        return forwarded;
    }
    let Some(upgrade) = council.desired_state().await.active_upgrade else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no upgrade in progress" })),
        )
            .into_response();
    };
    let upgrade_id = upgrade.upgrade_id.clone();
    let aborted = match crate::upgrade::orchestrator::abort(upgrade, "aborted by the operator") {
        Ok(aborted) => aborted,
        Err(e) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    if let Err(resp) = archive_aborted_upgrade(council, aborted).await {
        return resp;
    }
    Json(serde_json::json!({ "status": "aborted", "upgrade_id": upgrade_id })).into_response()
}

/// Start a cluster-wide rolling rollback (admin, leader only). The
/// binaries are already on every node's disk, so there is no registry or
/// signature material — just a target version and the node list.
///
/// A paused run is replaced: it is archived as aborted and the rollback
/// walks every node, moved or not, to the target.
pub(super) async fn upgrade_cluster_rollback_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    #[derive(serde::Deserialize)]
    struct RollbackRequest {
        target_version: crate::upgrade::BinaryVersion,
        nodes: Vec<StartUpgradeNode>,
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": crate::upgrade::NO_COUNCIL })),
        )
            .into_response();
    };
    if let Some(forwarded) = forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        "/v1/upgrade/cluster-rollback",
        &headers,
        &body,
    )
    .await
    {
        return forwarded;
    }
    let request: RollbackRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid request: {e}") })),
            )
                .into_response();
        }
    };
    let paused = match council.desired_state().await.active_upgrade {
        None => None,
        Some(active) => match crate::upgrade::orchestrator::supersede(
            active.clone(),
            &format!("replaced by a rollback to {}", request.target_version),
        ) {
            Ok(superseded) => Some(superseded),
            Err(_) => return upgrade_in_progress(&active),
        },
    };

    // Validate each rollback node's identity against the authoritative gossip /
    // Raft view, exactly as upgrade_start does (M13/UPG2). The old rollback path
    // copied client-supplied node_id/address/role straight into the replicated
    // plan, so a caller could point the orchestrator at spoofed addresses or
    // roles that UPG2 exists to reject.
    let authoritative = match build_authoritative_view(&state, council).await {
        Ok(view) => view,
        Err(resp) => return resp,
    };
    let requested: Vec<crate::upgrade::plan::RequestedNode> = request
        .nodes
        .iter()
        .map(|node| crate::upgrade::plan::RequestedNode {
            node_id: node.node_id.clone(),
            address: node.address.clone(),
            role: node.role,
        })
        .collect();
    let derived_nodes = match crate::upgrade::plan::derive_upgrade_nodes(&requested, |id| {
        authoritative.get(id).cloned()
    }) {
        Ok(nodes) => nodes,
        Err(e) => return plan_error_response(&e),
    };

    // A rollback execs a binary each node already holds; nothing is
    // downloaded. Ask every node what its store holds and refuse here,
    // naming each node without the target, rather than record a run the
    // first such node refuses (#339).
    let stored: Vec<crate::upgrade::plan::StoredBinaries> =
        probe_planned_nodes(&state, &derived_nodes)
            .await
            .into_iter()
            .map(|(node, probe)| crate::upgrade::plan::StoredBinaries {
                node,
                running: probe.version,
                installed: probe.installed_versions,
            })
            .collect();
    if let Err(e) = crate::upgrade::plan::check_rollback_target(&request.target_version, &stored) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    if let Err(resp) = check_council_can_roll(council) {
        return resp;
    }

    let upgrade_id = format!(
        "rollback-{}-{}",
        request.target_version,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default()
    );
    let upgrade = crate::upgrade::types::ClusterUpgradeState {
        upgrade_id: upgrade_id.clone(),
        target_version: request.target_version,
        binaries: Vec::new(),
        parallel: 1,
        direction: crate::upgrade::types::UpgradeDirection::Rollback,
        phase: crate::upgrade::types::ClusterUpgradePhase::Preparing,
        registry_address: String::new(),
        allow_downgrade: false,
        nodes: derived_nodes,
    };

    // Archive the paused run only once the rollback plan is valid, so a
    // malformed request leaves it where it was.
    if let Some(superseded) = paused
        && let Err(resp) = archive_aborted_upgrade(council, superseded).await
    {
        return resp;
    }

    match council
        .write(crate::council::types::RaftRequest::UpgradeUpdate {
            state: Box::new(upgrade),
        })
        .await
    {
        Ok(_) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "rolling back", "upgrade_id": upgrade_id })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
