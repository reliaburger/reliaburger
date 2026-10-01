//! The apply route, and its forwarding to the council leader.

use super::*;

/// Deploy workloads, streaming progress via SSE.
///
/// Returns a Server-Sent Events stream. Each event's `data` field
/// contains a JSON-serialised `ApplyEvent`. The stream ends after
/// the `Complete` or `Error` event.
pub(super) const CAPACITY_PROBE_HEADER: &str = "x-reliaburger-capacity-probe";

pub(super) async fn apply_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    capacity_admission: Option<axum::Extension<crate::cluster::capacity::CapacityAdmission>>,
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    let mut config = match Config::parse(&body) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    if let Err(e) = config.validate() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    let rerun_jobs = match headers.get("x-reliaburger-rerun-jobs") {
        None => false,
        Some(value) if value.as_bytes() == b"acknowledged" => true,
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "x-reliaburger-rerun-jobs must equal acknowledged",
            )
                .into_response();
        }
    };
    if rerun_jobs {
        if let Err(error) = crate::bun::jobs::validate_rerun(&config) {
            return (StatusCode::BAD_REQUEST, error).into_response();
        }
        if let Err(response) = crate::sesame::auth::authorize_user(
            auth.as_deref(),
            crate::sesame::types::ApiRole::Deployer,
        ) {
            return response;
        }
    }

    let lease_id = match headers.get("x-reliaburger-test-lease") {
        Some(value) => match value.to_str() {
            Ok(value) if !value.is_empty() => Some(value.to_string()),
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    "x-reliaburger-test-lease must contain a lease id",
                )
                    .into_response();
            }
        },
        None => None,
    };
    let capacity_probe = match headers.get(CAPACITY_PROBE_HEADER) {
        Some(value) if value.as_bytes() == b"acknowledged" => true,
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "x-reliaburger-capacity-probe must equal acknowledged",
            )
                .into_response();
        }
        None => false,
    };
    if capacity_probe && lease_id.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "capacity probe requires x-reliaburger-test-lease",
        )
            .into_response();
    }
    if capacity_probe {
        if config.app.len() != 1
            || !config.job.is_empty()
            || !config.namespace.is_empty()
            || !config.permission.is_empty()
            || !config.build.is_empty()
            || config
                .app
                .values()
                .any(|spec| spec.replicas != crate::config::Replicas::Fixed(1))
        {
            return (
                StatusCode::BAD_REQUEST,
                "capacity probe requires exactly one new app with one replica",
            )
                .into_response();
        }
        let Some(auth) = auth.as_deref() else {
            return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        };
        if let Err(error) = state.static_capabilities.test_policy.authorise(
            crate::testkit::safety::OperationPermission::SaturateCapacity,
            &crate::testkit::safety::OperationAuthorisation {
                principal: &auth.token_name,
                role: auth.role,
                acknowledged: true,
            },
        ) {
            return (StatusCode::FORBIDDEN, error.to_string()).into_response();
        }
    }
    let mut lease_owner_id = None;
    let mut image_lease = None;
    if let Some(lease_id) = &lease_id {
        let Some(auth) = auth.as_deref() else {
            return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        };
        let Some(lease) = find_test_lease(&state, lease_id).await else {
            return lease_error_response(crate::testkit::lease::LeaseError::NotFound);
        };
        let wrong_kind = match lease.scope {
            LeaseScope::Applications => !config.job.is_empty(),
            LeaseScope::NodeJobs => {
                config.job.is_empty() || !config.app.is_empty() || !config.namespace.is_empty()
            }
        };
        if wrong_kind || !config.permission.is_empty() || !config.build.is_empty() {
            return lease_error_response(crate::testkit::lease::LeaseError::InvalidScope);
        }
        if auth.token_name != crate::sesame::auth::SYSTEM_PRINCIPAL
            && lease.owner_id != auth.principal_id
        {
            return lease_error_response(crate::testkit::lease::LeaseError::WrongOwner);
        }
        if !lease.is_active_at(crate::testkit::lease::now_unix_millis()) {
            return lease_error_response(crate::testkit::lease::LeaseError::NotActive);
        }
        if config
            .namespace
            .keys()
            .any(|namespace| namespace != &lease.namespace)
        {
            return lease_error_response(crate::testkit::lease::LeaseError::NamespaceMismatch);
        }
        for spec in config.app.values_mut() {
            match &spec.namespace {
                Some(namespace) if namespace != &lease.namespace => {
                    return lease_error_response(
                        crate::testkit::lease::LeaseError::NamespaceMismatch,
                    );
                }
                Some(_) => {}
                None => spec.namespace = Some(lease.namespace.clone()),
            }
        }
        for spec in config.job.values_mut() {
            match &spec.namespace {
                Some(namespace) if namespace != &lease.namespace => {
                    return lease_error_response(
                        crate::testkit::lease::LeaseError::NamespaceMismatch,
                    );
                }
                Some(_) => {}
                None => spec.namespace = Some(lease.namespace.clone()),
            }
        }
        lease_owner_id = Some(lease.owner_id.clone());
        image_lease = Some(lease);
    } else {
        if config
            .namespace
            .keys()
            .any(|namespace| crate::testkit::lease::valid_test_namespace(namespace))
        {
            return (
                StatusCode::CONFLICT,
                "test lease namespace requires x-reliaburger-test-lease",
            )
                .into_response();
        }
        for namespace in config
            .app
            .values()
            .map(|spec| spec.namespace.as_deref())
            .chain(config.job.values().map(|spec| spec.namespace.as_deref()))
        {
            let namespace = namespace.unwrap_or("default");
            if crate::testkit::lease::valid_test_namespace(namespace) {
                return (
                    StatusCode::CONFLICT,
                    "test lease namespace requires x-reliaburger-test-lease",
                )
                    .into_response();
            }
        }
    }

    // Ordinary namespace quotas and permission grants are operator policy.
    // A test lease has already confined its namespace declaration to the
    // caller-owned reservation above; its quota cannot affect other tenants.
    if !config.permission.is_empty() || (lease_id.is_none() && !config.namespace.is_empty()) {
        if let Err(response) = crate::sesame::auth::authorize_user(
            auth.as_deref(),
            crate::sesame::types::ApiRole::Admin,
        ) {
            return response;
        }
        if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
            return response;
        }
        if let Err(response) = enforce_cluster_permission(
            &state,
            auth.as_deref(),
            crate::config::PermissionAction::Admin,
        )
        .await
        {
            return response;
        }
    }

    let images = config
        .app
        .values()
        .flat_map(crate::config::AppSpec::image_references)
        .chain(config.job.values().filter_map(|job| job.image.as_deref()));
    if let Err(error) = crate::testkit::lease::authorise_image_references(
        images,
        image_lease
            .as_ref()
            .map(|lease| (lease, crate::testkit::lease::now_unix_millis())),
    ) {
        return lease_error_response(error);
    }

    // Check every workload before any Raft write or agent command. A job in
    // a mixed manifest must not bypass admission after its apps have committed.
    // Host execution includes both explicit binaries and inline scripts.
    let permissions = permission_map(&state).await;
    let targets = config
        .app
        .iter()
        .map(|(name, spec)| {
            (
                name.as_str(),
                spec.namespace.as_deref().unwrap_or("default"),
                spec.script.is_some() || spec.exec.is_some(),
            )
        })
        .chain(config.job.iter().map(|(name, spec)| {
            (
                name.as_str(),
                spec.namespace.as_deref().unwrap_or("default"),
                spec.script.is_some() || spec.exec.is_some(),
            )
        }));
    for (app_name, namespace, host_execution) in targets {
        if let Err(resp) =
            crate::sesame::auth::authorize_scoped(auth.as_deref(), app_name, namespace)
        {
            return resp;
        }
        if let Err(resp) = crate::sesame::auth::authorize_permission(
            auth.as_deref(),
            crate::config::PermissionAction::Deploy,
            app_name,
            namespace,
            &permissions,
        ) {
            return resp;
        }
        if host_execution
            && let Err(resp) = crate::sesame::auth::authorize_permission(
                auth.as_deref(),
                crate::config::PermissionAction::HostExec,
                app_name,
                namespace,
                &permissions,
            )
        {
            return resp;
        }
    }

    // Cluster mode (L1): apps, namespaces and permissions become desired
    // state in Raft; the leader schedules apps and every node's reconciler
    // converges. Jobs stay on the receiving node (cluster-wide job
    // scheduling is later work). A namespace/permission-only config still
    // routes through the cluster path so its resources are committed.
    if let Some(council) = &state.council
        && (!config.app.is_empty() || !config.namespace.is_empty() || !config.permission.is_empty())
    {
        return cluster_apply(
            state.clone(),
            Arc::clone(council),
            config,
            body,
            lease_id,
            headers,
            capacity_admission.map(|extension| extension.0),
        )
        .await;
    }
    if capacity_probe {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "capacity admission requires a live cluster scheduler",
        )
            .into_response();
    }

    let lease_operation = if let (Some(lease_id), Some(owner_id)) =
        (&lease_id, lease_owner_id.as_deref())
    {
        let now = crate::testkit::lease::now_unix_millis();
        let result = if is_node_job_lease(lease_id) {
            let job_ids = config
                .job
                .iter()
                .map(|(name, spec)| {
                    crate::meat::AppId::new(name, spec.namespace.as_deref().unwrap_or("default"))
                })
                .collect();
            state
                .local_test_leases
                .begin_job_operation(lease_id, owner_id, job_ids, now)
                .await
        } else {
            let app_ids = config
                .app
                .iter()
                .map(|(name, spec)| {
                    crate::meat::AppId::new(name, spec.namespace.as_deref().unwrap_or("default"))
                })
                .collect();
            state
                .local_test_leases
                .begin_app_operation(lease_id, owner_id, app_ids, now)
                .await
        };
        match result {
            Ok(operation) => Some(operation),
            Err(error) => return lease_error_response(error),
        }
    } else {
        None
    };

    let (agent_event_tx, mut agent_event_rx) = mpsc::channel::<ApplyEvent>(32);
    let command = if rerun_jobs {
        AgentCommand::RerunJobs {
            config,
            events: agent_event_tx,
        }
    } else {
        AgentCommand::Deploy {
            config,
            events: agent_event_tx,
        }
    };
    if state.cmd_tx.send(command).await.is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent unavailable" })),
        )
            .into_response();
    }

    let event_rx = if let Some(operation) = lease_operation {
        let (client_event_tx, client_event_rx) = mpsc::channel::<ApplyEvent>(32);
        // Keep consuming agent progress even when the HTTP client disconnects.
        // The per-lease guard prevents expiry cleanup from overtaking a deploy
        // which the agent has accepted but not completed yet.
        tokio::spawn(async move {
            let mut operation = Some(operation);
            while let Some(event) = agent_event_rx.recv().await {
                let terminal = matches!(
                    event,
                    ApplyEvent::Complete { .. } | ApplyEvent::Error { .. }
                );
                match client_event_tx.try_send(event) {
                    Ok(()) => {
                        if terminal {
                            operation.take();
                        }
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Full(event)) if terminal => {
                        // The deploy is over, so cleanup may proceed even if a
                        // slow client still needs time to accept its terminal
                        // event. Progress events may be coalesced under this
                        // backpressure, but the outcome is never dropped.
                        operation.take();
                        let _ = client_event_tx.send(event).await;
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
                }
            }
        });
        client_event_rx
    } else {
        agent_event_rx
    };

    let stream = ReceiverStream::new(event_rx).map(|apply_event| {
        let json = serde_json::to_string(&apply_event).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(Event::default().data(json))
    });

    Sse::new(stream).into_response()
}

/// Apply a config in cluster mode: propose each app spec to Raft.
///
/// On a follower, the whole request is forwarded to the leader's API
/// (openraft does not forward client writes), streaming its SSE
/// response back verbatim. Jobs in the same config still deploy on the
/// receiving node after the specs commit.
pub(super) async fn cluster_apply(
    state: ApiState,
    council: Arc<crate::council::CouncilNode>,
    config: Config,
    raw_body: String,
    lease_id: Option<String>,
    caller_headers: HeaderMap,
    capacity_admission: Option<crate::cluster::capacity::CapacityAdmission>,
) -> Response {
    // Follower? Forward to the leader rather than half-failing.
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
        let mut request = state
            .cluster_http
            .client()
            .post(format!("{leader_url}/v1/apply"))
            .body(raw_body);
        if let Some(lease_id) = &lease_id {
            request = request.header("x-reliaburger-test-lease", lease_id);
            if let Some(value) = caller_headers.get(CAPACITY_PROBE_HEADER) {
                request = request.header(CAPACITY_PROBE_HEADER, value.as_bytes());
            }
        }
        // The leader must evaluate the user's current grants, not the
        // follower's internal service identity. ClusterHttp has no default
        // bearer; node-to-node requests attach theirs explicitly.
        request = copy_forwarded_auth(request, &caller_headers);
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(5), request.send()).await;
        return match response {
            Ok(Ok(response)) => {
                let mut builder = Response::builder().status(response.status());
                if let Some(content_type) = response.headers().get(axum::http::header::CONTENT_TYPE)
                {
                    builder = builder.header(axum::http::header::CONTENT_TYPE, content_type);
                }
                builder
                    .body(axum::body::Body::from_stream(response.bytes_stream()))
                    .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
            }
            Err(_) => (
                StatusCode::GATEWAY_TIMEOUT,
                "leader apply request timed out",
            )
                .into_response(),
            Ok(Err(e)) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": format!("failed to forward apply to the leader: {e}")
                })),
            )
                .into_response(),
        };
    }

    // Permissions and builds may target a namespace an earlier apply
    // created, not just one in this file. Validate against the union of
    // this config's namespaces and those already committed, so a build
    // scoped to an existing namespace validates and one targeting a ghost
    // namespace is rejected before any write lands.
    let known_namespaces: Vec<String> = council
        .desired_state()
        .await
        .namespaces
        .keys()
        .cloned()
        .collect();
    if let Err(e) = config.validate_against(&known_namespaces) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    if caller_headers.contains_key(CAPACITY_PROBE_HEADER) {
        use crate::cluster::capacity::{CapacityAdmissionError, SchedulingRefusal};
        let Some(admission) = capacity_admission else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "capacity admission is unavailable",
            )
                .into_response();
        };
        let Some((name, spec)) = config.app.iter().next() else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let app_id = crate::meat::AppId::new(name, spec.namespace.as_deref().unwrap_or("default"));
        let outcome = admission.check(&app_id, spec).await;
        if !council.is_leader().await {
            return unavailable_response("leadership changed during capacity admission".into());
        }
        let active_lease = council.desired_state().await.test_leases;
        if !lease_id
            .as_ref()
            .and_then(|id| active_lease.get(id))
            .is_some_and(|lease| lease.is_active_at(crate::testkit::lease::now_unix_millis()))
        {
            return lease_error_response(crate::testkit::lease::LeaseError::NotActive);
        }
        match outcome {
            Ok(()) => {}
            Err(CapacityAdmissionError::Rejected(error)) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(SchedulingRefusal { error }),
                )
                    .into_response();
            }
            Err(error) => return unavailable_response(error.to_string()),
        }
    }

    let (event_tx, event_rx) = mpsc::channel::<ApplyEvent>(32);
    let cmd_tx = state.cmd_tx.clone();
    tokio::spawn(async move {
        let mut committed = 0usize;
        // The one shared path: namespaces, then permissions, then apps.
        // Lettuce writes the exact same set for the same config, so manual
        // apply and GitOps can't diverge (12b.2 T6). A failed write is a
        // hard stop — half an apply leaves desired state inconsistent.
        let writes = match &lease_id {
            Some(lease_id) => match crate::council::config_to_leased_writes(
                &config,
                lease_id,
                crate::testkit::lease::now_unix_millis(),
            ) {
                Ok(writes) => writes,
                // A leased apply that declares a non-owned kind (job, build,
                // permission) is rejected outright rather than silently
                // dropping it — see `config_to_leased_writes`.
                Err(e) => {
                    let _ = event_tx
                        .send(ApplyEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }
            },
            None => crate::council::config_to_desired_writes(&config),
        };
        for request in writes {
            let describe = describe_write(&request);
            match council.write(request).await {
                // A state-machine refusal (lease expired, in cleanup, resource
                // owned elsewhere, quota) is NOT a commit — surfacing it as an
                // error stops the apply instead of streaming "committed" and
                // letting the case die later as Unknown(TimedOut).
                Ok(crate::council::CouncilResponse::Refused { reason }) => {
                    let _ = event_tx
                        .send(ApplyEvent::Error {
                            message: format!("{describe}: refused by the cluster: {reason}"),
                        })
                        .await;
                    return;
                }
                Ok(_) => {
                    committed += 1;
                    let _ = event_tx
                        .send(ApplyEvent::Progress {
                            message: format!("{describe}: committed to the cluster"),
                        })
                        .await;
                }
                Err(e) => {
                    let _ = event_tx
                        .send(ApplyEvent::Error {
                            message: format!("{describe}: raft write failed: {e}"),
                        })
                        .await;
                    return;
                }
            }
        }

        // Jobs are not cluster-scheduled yet; run them here, as before.
        if !config.job.is_empty() {
            let _ = event_tx
                .send(ApplyEvent::Progress {
                    message: format!(
                        "{} job(s) deploying on this node (jobs are not cluster-scheduled yet)",
                        config.job.len()
                    ),
                })
                .await;
            let job_config = Config {
                job: config.job.clone(),
                ..Config::default()
            };
            let _ = cmd_tx
                .send(AgentCommand::Deploy {
                    config: job_config,
                    events: event_tx.clone(),
                })
                .await;
            // The agent sends Complete/Error for the job deploy.
            return;
        }

        let _ = event_tx
            .send(ApplyEvent::Complete {
                created: committed,
                instances: vec![],
            })
            .await;
    });

    let stream = ReceiverStream::new(event_rx).map(|apply_event| {
        let json = serde_json::to_string(&apply_event).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(Event::default().data(json))
    });
    Sse::new(stream).into_response()
}

/// A short human-readable label for an apply progress message.
pub(super) fn describe_write(request: &crate::council::types::RaftRequest) -> String {
    use crate::council::types::RaftRequest;
    match request {
        RaftRequest::AppSpec { app_id, .. } => format!("app {}", app_id.name),
        RaftRequest::NamespaceSpec { name, .. } => format!("namespace {name}"),
        RaftRequest::PermissionSpec { name, .. } => format!("permission {name}"),
        RaftRequest::TestLeaseAppSpec { app_id, .. } => {
            format!("leased app {}", app_id.name)
        }
        RaftRequest::TestLeaseNamespaceSpec { name, .. } => {
            format!("leased namespace {name}")
        }
        _ => "resource".to_string(),
    }
}

/// Resolve the current leader's API base URL.
///
/// Preferred source is the gossip-fed membership table (it stores real
/// per-node API addresses); the fallback derives from the leader's
/// raft IP and this node's own API port, which is correct only when
/// ports are uniform across the cluster.
pub(crate) async fn leader_api_url(
    state: &ApiState,
    council: &crate::council::CouncilNode,
) -> Option<String> {
    let leader_id = council.current_leader().await?;
    let (leader_name, leader_ip) = {
        let metrics = council.metrics();
        let metrics = metrics.borrow();
        let info = metrics
            .membership_config
            .membership()
            .get_node(&leader_id)?;
        (info.name.clone(), info.addr.ip())
    };

    if let Some(membership) = &state.membership {
        let members = membership.read().await;
        if let Some(info) = members
            .iter()
            .find(|m| m.node_id == crate::meat::NodeId::new(&leader_name))
        {
            return Some(state.cluster_http.url(&info.address.to_string(), ""));
        }
    }

    Some(
        state
            .cluster_http
            .url(&format!("{leader_ip}:{}", state.api_port), ""),
    )
}
