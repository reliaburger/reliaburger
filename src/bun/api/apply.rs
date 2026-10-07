//! The apply route, and its forwarding to the council leader.

use super::*;
use crate::pickle::binding::{AppliedBinding, BindError, ImageBinder};

/// Bind `config`'s images before the request is answered, for the paths
/// that must know the bound config before their stream starts (a
/// standalone deploy, a cluster apply with jobs). A refusal is the HTTP
/// response: 400 for a reference that doesn't parse, 403 for an image the
/// node's upstream trust rules refuse, 502 when the registry couldn't name a
/// digest and the cache doesn't hold the tag.
#[allow(clippy::result_large_err)]
pub(crate) async fn bind_images(
    state: &ApiState,
    binder: &ImageBinder,
    config: &mut Config,
) -> Result<Vec<AppliedBinding>, Response> {
    let catalog = binding_catalog(state).await;
    binder.bind_config(config, &catalog).await.map_err(|error| {
        let status = match error {
            BindError::InvalidReference { .. } => StatusCode::BAD_REQUEST,
            BindError::Unresolved { .. } => StatusCode::BAD_GATEWAY,
            BindError::NotAllowed(_) => StatusCode::FORBIDDEN,
        };
        (
            status,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response()
    })
}

/// The catalogue a binding resolves Pickle images against: the council's,
/// which every node shares, or this node's own when it runs standalone.
async fn binding_catalog(state: &ApiState) -> ManifestCatalog {
    match (&state.council, &state.pickle_catalog) {
        (Some(council), _) => council.manifest_catalog().await,
        (None, Some(catalog)) => catalog.read().await.clone(),
        (None, None) => ManifestCatalog::default(),
    }
}

/// Deploy workloads, streaming progress via SSE.
///
/// Returns a Server-Sent Events stream. Each event's `data` field
/// contains a JSON-serialised `ApplyEvent`. The stream ends after
/// the `Complete` or `Error` event.
pub(super) const CAPACITY_PROBE_HEADER: &str = "x-reliaburger-capacity-probe";

pub(super) async fn apply_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    capacity_admission: Option<axum::Extension<crate::cluster::capacity::CapacityAdmission>>,
    binder: Option<axum::Extension<ImageBinder>>,
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

    let validation = if state.council.is_some() {
        config.validate_intrinsic()
    } else {
        config.validate()
    };
    if let Err(e) = validation {
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
        if let Err(response) = crate::sesame::auth::authorize_workload(
            auth.as_deref(),
            app_name,
            namespace,
            host_execution,
            &permissions,
        ) {
            return response;
        }
    }

    let identities: Vec<(String, String)> = config
        .app
        .iter()
        .map(|(name, spec)| {
            (
                name.clone(),
                spec.namespace.clone().unwrap_or_else(|| "default".into()),
            )
        })
        .chain(config.job.iter().map(|(name, spec)| {
            (
                name.clone(),
                spec.namespace.clone().unwrap_or_else(|| "default".into()),
            )
        }))
        .collect();

    if lease_id.is_none() && !config.job.is_empty() {
        return crate::bun::job_apply::apply(
            state,
            auth.as_deref(),
            binder.as_deref(),
            config,
            &headers,
            body,
            rerun_jobs,
        )
        .await;
    }
    let _standalone_gate = if state.council.is_none() {
        Some(state.task_arrays.apply_gate.clone().lock_owned().await)
    } else {
        None
    };
    let common = crate::bun::task_array_leader::read_task_arrays(&state).await;
    if config.app.iter().any(|(name, spec)| {
        common
            .deployment_owner(name, spec.namespace.as_deref().unwrap_or("default"))
            .is_some()
            || common.job_identity_reserved(spec.namespace.as_deref().unwrap_or("default"), name)
    }) {
        return (
            StatusCode::CONFLICT,
            "app identity belongs to an unsettled deployment, job definition or retained run",
        )
            .into_response();
    }

    if !identities.is_empty() {
        let ownership = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            if let Some(council) = &state.council {
                let desired = council.desired_state().await;
                if identities.iter().any(|(name, namespace)| {
                    desired
                        .batch_state
                        .execution_owner(namespace, name)
                        .is_some()
                }) {
                    return Err((
                        StatusCode::CONFLICT,
                        "workload identity belongs to a retained batch execution",
                    )
                        .into_response());
                }
            }
            let owned = match ask_agent_bounded(&state.cmd_tx, |response| {
                AgentCommand::BatchOwnedExecutions {
                    identities,
                    response,
                }
            })
            .await
            {
                Ok(owned) => owned,
                Err(response) => return Err(response),
            };
            if !owned.is_empty() {
                return Err((
                    StatusCode::CONFLICT,
                    "workload identity belongs to a retained local batch execution",
                )
                    .into_response());
            }
            Ok(())
        })
        .await;
        match ownership {
            Ok(Ok(())) => {}
            Ok(Err(response)) => return response,
            Err(_) => return agent_unavailable(),
        }
    }

    // Cluster mode (L1): apps, namespaces and permissions become desired
    // state in Raft; the leader schedules apps and every node's reconciler
    // converges. Jobs stay on the receiving node (cluster-wide job
    // scheduling is later work). A namespace/permission-only config still
    // routes through the cluster path so its resources are committed. Build-only
    // manifests also need the leader's namespace context before acceptance.
    if let Some(council) = &state.council
        && (!config.app.is_empty()
            || !config.namespace.is_empty()
            || !config.permission.is_empty()
            || !config.build.is_empty()
            || (lease_id.is_none() && !config.job.is_empty()))
    {
        return cluster_apply(
            state.clone(),
            Arc::clone(council),
            config,
            body,
            lease_id,
            headers,
            capacity_admission.map(|extension| extension.0),
            binder.map(|extension| extension.0),
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

    // A standalone node binds here, before its agent deploys, so a single
    // node gets the same guarantee as a cluster: restarts run the bytes the
    // apply resolved.
    let bindings = match &binder {
        Some(axum::Extension(binder)) => match bind_images(&state, binder, &mut config).await {
            Ok(bindings) => bindings,
            Err(response) => return response,
        },
        None => Vec::new(),
    };

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

    // Room for the binding lines, which go out before the agent's events.
    let (agent_event_tx, mut agent_event_rx) = mpsc::channel::<ApplyEvent>(32 + bindings.len());
    for binding in &bindings {
        let _ = agent_event_tx
            .send(ApplyEvent::Progress {
                message: binding.to_string(),
            })
            .await;
    }
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

    let event_rx = {
        let (client_event_tx, client_event_rx) = mpsc::channel::<ApplyEvent>(32);
        // Keep consuming agent progress even when the HTTP client disconnects.
        // The per-lease guard prevents expiry cleanup from overtaking a deploy
        // which the agent has accepted but not completed yet.
        tokio::spawn(async move {
            let mut operation = lease_operation;
            let _gate = _standalone_gate;
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
#[allow(clippy::too_many_arguments)]
pub(super) async fn cluster_apply(
    state: ApiState,
    council: Arc<crate::council::CouncilNode>,
    config: Config,
    raw_body: String,
    lease_id: Option<String>,
    caller_headers: HeaderMap,
    capacity_admission: Option<crate::cluster::capacity::CapacityAdmission>,
    binder: Option<ImageBinder>,
) -> Response {
    // Follower? Forward to the leader rather than half-failing. The leader
    // binds images: a follower's binder (or lack of one) plays no part.
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
        if let Some(value) = caller_headers.get("x-reliaburger-rerun-jobs") {
            request = request.header("x-reliaburger-rerun-jobs", value.as_bytes());
        }
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

    let desired = council.desired_state().await;
    if let Some(owner) =
        crate::council::prerequisites::conflict(&desired.prerequisite_claims, &config)
    {
        return (StatusCode::CONFLICT, format!("prerequisite operation {owner} still owns a workload; its outcome must be established before another apply")).into_response();
    }
    if config.job.values().any(|job| job.schedule.is_some()) {
        return (StatusCode::UNPROCESSABLE_ENTITY,
            "cluster apply cannot safely own recurring schedules; use a standalone node for cron jobs").into_response();
    }
    if !config.job.is_empty() {
        // The prerequisite claim records the config in Raft before its
        // stream starts, so these images bind before the claim is written.
        let mut config = config;
        let bindings = match &binder {
            Some(binder) => match bind_images(&state, binder, &mut config).await {
                Ok(bindings) => bindings,
                Err(response) => return response,
            },
            None => Vec::new(),
        };
        return cluster_prerequisite_apply(state, council, config, &caller_headers, bindings).await;
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
        let mut config = config;
        // Bind inside the stream, so a slow registry shows as a wait in
        // `relish apply` rather than as a follower's forward timing out.
        if let Some(binder) = &binder {
            let catalog = council.manifest_catalog().await;
            match binder.bind_config(&mut config, &catalog).await {
                Ok(bindings) => {
                    for binding in bindings {
                        let _ = event_tx
                            .send(ApplyEvent::Progress {
                                message: binding.to_string(),
                            })
                            .await;
                    }
                }
                Err(error) => {
                    let _ = event_tx
                        .send(ApplyEvent::Error {
                            message: error.to_string(),
                        })
                        .await;
                    return;
                }
            }
        }
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

/// A proposal deadline bounds the caller, never proves that Raft did not commit.
/// Uncertain admission cannot dispatch work or release a replicated ownership fence.
async fn write_job_claim(
    council: &crate::council::CouncilNode,
    request: crate::council::RaftRequest,
) -> Result<crate::council::CouncilResponse, String> {
    tokio::time::timeout(std::time::Duration::from_secs(5), council.write(request))
        .await
        .map_err(|_| {
            "job ownership write timed out; outcome unknown and ownership may remain held"
                .to_string()
        })?
        .map_err(|error| error.to_string())
}

/// Acquire durable intent before spawning the worker; conflicts are HTTP 409,
/// while execution errors are terminal SSE events under the retained claim.
async fn cluster_prerequisite_apply(
    state: ApiState,
    council: Arc<crate::council::CouncilNode>,
    config: Config,
    caller_headers: &HeaderMap,
    bindings: Vec<crate::pickle::binding::AppliedBinding>,
) -> Response {
    let rerun_jobs = caller_headers
        .get("x-reliaburger-rerun-jobs")
        .is_some_and(|value| value.as_bytes() == b"acknowledged");
    use crate::bun::deploy_operations::DeployOperationOutcome;
    use crate::council::{CouncilResponse, RaftRequest};
    let term = council.current_term();
    let (response, answer) = oneshot::channel();
    let prepared = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        state
            .cmd_tx
            .send(AgentCommand::PreparePrerequisites { config, response })
            .await
            .map_err(|_| "agent unavailable during prerequisite preparation".to_string())?;
        answer
            .await
            .map_err(|_| "prerequisite preparation disappeared".to_string())?
    })
    .await;
    let (config, operation) = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(message)) => return (StatusCode::CONFLICT, message).into_response(),
        Err(_) => {
            return unavailable_response("prerequisite preparation timed out before launch".into());
        }
    };
    let operation_id = hex::encode(rand::random::<[u8; 16]>());
    match write_job_claim(
        &council,
        RaftRequest::PrerequisiteBegin {
            operation_id: operation_id.clone(),
            term,
            config: Box::new(config.clone()),
        },
    )
    .await
    {
        Ok(CouncilResponse::Refused { reason }) => {
            operation
                .finish(DeployOperationOutcome::Failed, reason.clone())
                .await;
            return (StatusCode::CONFLICT, reason).into_response();
        }
        Err(error) => {
            operation
                .finish(DeployOperationOutcome::Unknown, error.to_string())
                .await;
            return unavailable_response(error.to_string());
        }
        Ok(_) => {}
    }
    let (events, event_rx) = mpsc::channel::<ApplyEvent>(32);
    tokio::spawn(async move {
        for binding in bindings {
            let _ = events
                .send(ApplyEvent::Progress {
                    message: binding.to_string(),
                })
                .await;
        }
        let result: Result<usize, String> = async {
            if !council.is_leader().await || council.current_term() != term {
                return Err(
                    "leadership changed before prerequisite dispatch; ownership remains held"
                        .into(),
                );
            }
            if config.job.values().any(|job| !job.run_before.is_empty()) {
                let (response, answer) = oneshot::channel();
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    state.cmd_tx.send(AgentCommand::RunPrerequisites {
                        config: config.clone(),
                        operation: operation.clone(),
                        response,
                    }),
                )
                .await
                .map_err(|_| "agent prerequisite queue timed out; ownership remains held")?
                .map_err(
                    |_| "agent stopped before prerequisite dispatch; ownership remains held",
                )?;
                match answer
                    .await
                    .map_err(|_| "prerequisite worker disappeared; ownership remains held")?
                {
                    Ok(()) => {}
                    Err(failure) => {
                        if failure.settled {
                            // A positively observed and persisted nonzero exit may
                            // release intent. A different term cannot release it.
                            match write_job_claim(
                                &council,
                                RaftRequest::PrerequisiteFailed {
                                    operation_id: operation_id.clone(),
                                },
                            )
                            .await
                            {
                                Ok(CouncilResponse::Refused { reason }) => {
                                    return Err(format!(
                                        "{}; ownership remains held: {reason}",
                                        failure.message
                                    ));
                                }
                                Err(error) => return Err(format!("{}; {error}", failure.message)),
                                Ok(_) => {}
                            }
                        }
                        return Err(failure.message);
                    }
                }
            }
            // Cancellation may arrive after the actor's success response.
            // Once submitted, a Raft transaction cannot be undone by cancellation.
            if operation.cancellation_requested() {
                return Err(
                    "prerequisite cancelled before app publication; ownership remains held".into(),
                );
            }
            match write_job_claim(
                &council,
                RaftRequest::PrerequisiteCommit {
                    operation_id: operation_id.clone(),
                },
            )
            .await
            {
                Ok(CouncilResponse::Refused { reason }) => Err(reason),
                Err(error) => Err(error.to_string()),
                Ok(_) => Ok(crate::council::config_to_desired_writes(&config).len()),
            }
        }
        .await;
        operation
            .finish(
                if result.is_ok() {
                    DeployOperationOutcome::Completed
                } else {
                    DeployOperationOutcome::Unknown
                },
                "prerequisite execution and desired-state transaction settled",
            )
            .await;
        match result {
            Err(message) => {
                let _ = events.send(ApplyEvent::Error { message }).await;
            }
            Ok(created) => {
                let remaining: std::collections::BTreeMap<_, _> = config
                    .job
                    .into_iter()
                    .filter(|(_, spec)| spec.run_before.is_empty())
                    .collect();
                if remaining.is_empty() {
                    let _ = events
                        .send(ApplyEvent::Complete {
                            created,
                            instances: vec![],
                        })
                        .await;
                } else {
                    let config = Config {
                        job: remaining,
                        ..Config::default()
                    };
                    let (launch_events, mut launch_rx) = mpsc::channel(32);
                    let command = if rerun_jobs {
                        AgentCommand::RerunJobs {
                            config: config.clone(),
                            events: launch_events,
                        }
                    } else {
                        AgentCommand::Deploy {
                            config: config.clone(),
                            events: launch_events,
                        }
                    };
                    let _ = events
                        .send(ApplyEvent::Progress {
                            message: format!("{} job(s) deploying on this node", config.job.len()),
                        })
                        .await;
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        state.cmd_tx.send(command),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        _ => {
                            let _ = events.send(ApplyEvent::Error { message: "agent unavailable after app commitment; job ownership remains held".into() }).await;
                            return;
                        }
                    }
                    while let Some(event) = launch_rx.recv().await {
                        if matches!(event, ApplyEvent::Complete { .. }) {
                            // Capture before exposing startup completion. The separate
                            // watcher retains intent until a positive terminal outcome.
                            let (response, answer) = oneshot::channel();
                            let receipt =
                                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                                    state
                                        .cmd_tx
                                        .send(AgentCommand::CaptureClusterJobs {
                                            config: config.clone(),
                                            response,
                                        })
                                        .await
                                        .ok()?;
                                    answer.await.ok()?.ok()
                                })
                                .await
                                .ok()
                                .flatten();
                            if let Some(receipt) = receipt {
                                spawn_job_apply_settlement(
                                    state.cmd_tx.clone(),
                                    council.clone(),
                                    operation_id.clone(),
                                    receipt,
                                );
                            }
                            let _ = events.send(event).await;
                            return;
                        }
                        if matches!(event, ApplyEvent::Error { .. }) {
                            let _ = events.send(event).await;
                            return;
                        }
                        // Slow clients may coalesce progress, while terminal
                        // outcomes retain their delivery and ownership guarantees.
                        let _ = events.try_send(event);
                    }
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: "job worker disappeared; ownership remains held".into(),
                        })
                        .await;
                }
            }
        }
    });
    let stream = ReceiverStream::new(event_rx).map(|event| {
        Ok::<_, std::convert::Infallible>(
            Event::default().data(serde_json::to_string(&event).unwrap_or_default()),
        )
    });
    Sse::new(stream).into_response()
}

/// One bounded metadata request per tick, at most one watcher per held claim.
/// A missing actor, uncertain receipt or different leader retains the fence.
pub(super) fn spawn_job_apply_settlement(
    commands: mpsc::Sender<AgentCommand>,
    council: Arc<crate::council::CouncilNode>,
    operation_id: String,
    receipt: Arc<crate::bun::agent::ClusterJobReceipt>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (response, answer) = oneshot::channel();
            let observation = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                commands
                    .send(AgentCommand::ClusterJobsSettlement {
                        receipt: receipt.clone(),
                        response,
                    })
                    .await
                    .ok()?;
                answer.await.ok()
            })
            .await
            .ok()
            .flatten();
            match observation {
                Some(crate::bun::agent::ClusterJobSettlement::Terminal) => {
                    let _ = write_job_claim(
                        &council,
                        crate::council::RaftRequest::JobApplyComplete { operation_id },
                    )
                    .await;
                    return;
                }
                Some(crate::bun::agent::ClusterJobSettlement::Pending) => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                _ => return,
            }
        }
    })
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
