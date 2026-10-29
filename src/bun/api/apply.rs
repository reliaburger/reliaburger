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
    // `relish apply` expands templates before it sends a manifest; one that
    // arrives unexpanded expands here, before validation and policy, so
    // both see only ordinary apps (#1124).
    if let Err(error) = config.expand_templates() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response();
    }

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
        if let Err(error) = crate::bun::job_apply::validate_rerun(&config) {
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
            || !config.policy.is_empty()
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
        if wrong_kind
            || !config.permission.is_empty()
            || !config.policy.is_empty()
            || !config.build.is_empty()
        {
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

    // Ordinary namespace quotas, permission grants and policies are
    // operator policy. A test lease has already confined its namespace
    // declaration to the caller-owned reservation above; its quota cannot
    // affect other tenants.
    if !config.permission.is_empty()
        || !config.policy.is_empty()
        || (lease_id.is_none() && !config.namespace.is_empty())
    {
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

    // Policies live in the council, so a manifest that declares one needs
    // a cluster. Job manifests take the job lifecycle, which judges jobs by
    // the policies already committed (#1119) but doesn't write policies,
    // so a policy and a job go in separate applies.
    if !config.policy.is_empty() && (!config.job.is_empty() || state.council.is_none()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "policies apply only to a cluster, in a manifest without jobs"
            })),
        )
            .into_response();
    }

    // The record of this apply names the manifest's workloads and, when
    // there is just one, its namespace (#685), and every policy it writes
    // (#1117).
    crate::bun::audit::annotate(audit::manifest_audit_note(&config));

    // Check every workload before any Raft write or agent command. A job in
    // a mixed manifest must not bypass admission after its apps have committed.
    // Host execution includes explicit binaries, inline scripts and, for
    // apps, host-path volume or config-file sources: a bind-mounted host
    // path reaches past the namespace just as a host binary does, and so
    // does a runtime knob (#1122).
    let permissions = permission_map(&state).await;
    // The token gate for runtime knobs (#1122), checked first so its refusal
    // can name the knob. `needs_host_access` below counts knobs too, so the
    // workload check holds the same line. The node gate comes later, on the
    // node that runs the app.
    for (app_name, spec) in &config.app {
        let Some(knob) = spec.runtime_knob_uses().next() else {
            continue;
        };
        let namespace = spec.namespace.as_deref().unwrap_or("default");
        if crate::sesame::auth::authorize_permission(
            auth.as_deref(),
            crate::config::PermissionAction::HostExec,
            app_name,
            namespace,
            &permissions,
        )
        .is_err()
        {
            return (
                StatusCode::FORBIDDEN,
                format!(
                    "app.{app_name}: {knob} refused by the token gate: a runtime knob needs \
                     the host-exec permission, and this token's permission doesn't grant it"
                ),
            )
                .into_response();
        }
    }
    let targets = config
        .app
        .iter()
        .map(|(name, spec)| {
            (
                name.as_str(),
                spec.namespace.as_deref().unwrap_or("default"),
                spec.needs_host_access(),
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
    // Every job, leased or not, takes the common job lifecycle: the leader
    // admits it as an indexed run and grants it to a node with room.
    if !config.job.is_empty() {
        return crate::bun::job_apply::apply(
            state,
            auth.as_deref(),
            binder.as_deref(),
            config,
            &headers,
            body,
            rerun_jobs,
            image_lease.as_ref(),
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

    // Cluster mode (L1): apps, namespaces and permissions become desired
    // state in Raft; the leader schedules apps and every node's reconciler
    // converges. A namespace/permission-only config still routes through the
    // cluster path so its resources are committed. Build-only manifests also
    // need the leader's namespace context before acceptance.
    if let Some(council) = &state.council
        && (!config.app.is_empty()
            || !config.namespace.is_empty()
            || !config.permission.is_empty()
            || !config.policy.is_empty()
            || !config.build.is_empty())
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
            auth.map(|extension| extension.0),
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

    let lease_operation =
        if let (Some(lease_id), Some(owner_id)) = (&lease_id, lease_owner_id.as_deref()) {
            let app_ids = config
                .app
                .iter()
                .map(|(name, spec)| {
                    crate::meat::AppId::new(name, spec.namespace.as_deref().unwrap_or("default"))
                })
                .collect();
            match state
                .local_test_leases
                .begin_app_operation(
                    lease_id,
                    owner_id,
                    app_ids,
                    crate::testkit::lease::now_unix_millis(),
                )
                .await
            {
                Ok(operation) => Some(operation),
                Err(error) => return lease_error_response(error),
            }
        } else {
            None
        };

    // Room for the host-path notes and binding lines, which go out before
    // the agent's events.
    let notes: Vec<String> = host_path_notes(&config)
        .into_iter()
        .chain(runtime_knob_notes(&config))
        .chain(bindings.iter().map(ToString::to_string))
        .collect();
    let (agent_event_tx, mut agent_event_rx) = mpsc::channel::<ApplyEvent>(32 + notes.len());
    for message in notes {
        let _ = agent_event_tx.send(ApplyEvent::Progress { message }).await;
    }
    let command = AgentCommand::Deploy {
        config,
        events: agent_event_tx,
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

/// One progress line per host-path `source` in `config`: the API can't see
/// any node's `[storage] allowed_host_paths`, so it says what the node needs.
pub(crate) fn host_path_notes(config: &Config) -> Vec<String> {
    config
        .app
        .iter()
        .flat_map(|(name, spec)| {
            spec.host_path_sources().map(move |source| {
                format!(
                    "app.{name}: host path {} is mounted only on a node whose \
                     [storage] allowed_host_paths lists a prefix covering it",
                    source.display()
                )
            })
        })
        .collect()
}

/// One progress line per runtime knob in `config` (#1122): like a host
/// path, the API can't see any node's `[runtime]` allowlist, so it says
/// which key the node needs. The scheduler places the app only where one
/// lists it.
pub(crate) fn runtime_knob_notes(config: &Config) -> Vec<String> {
    config
        .app
        .iter()
        .flat_map(|(name, spec)| {
            spec.runtime_knob_uses().map(move |knob| {
                format!(
                    "app.{name}: {knob} runs only on a node whose [runtime] {} lists it",
                    knob.kind.allowlist_key()
                )
            })
        })
        .collect()
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
    mut config: Config,
    raw_body: String,
    lease_id: Option<String>,
    caller_headers: HeaderMap,
    capacity_admission: Option<crate::cluster::capacity::CapacityAdmission>,
    binder: Option<ImageBinder>,
    auth: Option<crate::sesame::auth::AuthContext>,
) -> Response {
    // Follower? Forward to the leader rather than half-failing. The leader
    // binds images: a follower's binder (or lack of one) plays no part.
    if !council.is_leader().await {
        let leader_url = match leader_api_url(&state, &council, &caller_headers).await {
            Ok(url) => url,
            Err(refusal) => return refusal.into_response(),
        };
        let mut request = mark_leader_forwarded(
            state
                .cluster_http
                .client()
                .post(format!("{leader_url}/v1/apply"))
                .body(raw_body),
        );
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

    // Jobs never reach this path: apply_handler sends every manifest with
    // jobs through the common job lifecycle (`job_apply::apply`).
    if !config.job.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "jobs apply through the common job lifecycle, not the app path",
        )
            .into_response();
    }

    // Resource owners (#1125): an app a controller owns is skipped unless
    // this apply declares that owner, and only the owner's token or an admin
    // may declare one. Decided before policy, so a skipped app is neither
    // judged nor written.
    let owner_notes =
        match skip_owned_apps(&council, &mut config, &owner_caller(auth.as_ref())).await {
            Ok(notes) => notes,
            Err(error) => return owner_refusal(&error),
        };

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

    // Policies (#1117): judge the declared apps before anything is written
    // or bound, under the policies in force with this config's own
    // replacing any of the same name. A denial writes nothing and is
    // audited, naming every policy. The writes are worked out again after
    // binding, below; only their fence generation is decided here.
    let caller = policy_caller(auth.as_ref());
    let (committed_policies, committed_generation) = council
        .read_desired(|desired| (desired.policies.clone(), desired.policy_generation))
        .await;
    let admission = match crate::policy::admit_blocking(
        committed_policies,
        committed_generation,
        config.clone(),
        caller.clone(),
        crate::policy::Source::Apply,
    )
    .await
    {
        Ok(admitted) => admitted,
        Err(error) => return policy_refusal(&state, auth.as_ref(), &error).await,
    };
    record_policy_audits(&state, auth.as_ref(), &admission.audits).await;
    // The apps as declared, for judging again, and the apps to write:
    // each with its policy defaults filled in (#1118).
    let declared = config.clone();
    let mut config = config;
    for (name, spec) in config.app.iter_mut() {
        let app_id = crate::meat::AppId::new(name, spec.namespace.as_deref().unwrap_or("default"));
        if let Some(app) = admission.defaulted.get(&app_id) {
            *spec = app.effective.clone();
        }
    }

    // A manual apply of a GitOps-managed resource goes ahead, with a warning
    // that the next sync puts back what git declares (#693).
    let gitops_warnings: Vec<String> = council
        .desired_state()
        .await
        .gitops_owned
        .managed_in(&config)
        .iter()
        .map(|resource| crate::lettuce::types::managed_apply_warning(resource))
        .collect();

    let (event_tx, event_rx) = mpsc::channel::<ApplyEvent>(32);
    tokio::spawn(async move {
        let mut config = config;
        let filled = admission.filled.iter().map(ToString::to_string);
        let policy_warnings = admission
            .warnings
            .iter()
            .map(|finding| format!("warning: {finding}"));
        for message in owner_notes
            .into_iter()
            .chain(gitops_warnings)
            .chain(filled)
            .chain(policy_warnings)
            .chain(host_path_notes(&config))
            .chain(runtime_knob_notes(&config))
        {
            let _ = event_tx.send(ApplyEvent::Progress { message }).await;
        }
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
        // Every app write is fenced on the policy generation it was judged
        // under. When the policies move first, the state machine refuses
        // it, and the apps still to write are judged again (#1117).
        let mut pending: std::collections::VecDeque<_> = crate::policy::fence(
            writes,
            admission.generation,
            &crate::policy::declared_apps(&declared),
        )
        .into();
        let mut policy_attempts = 0usize;
        while let Some(request) = pending.pop_front() {
            let describe = describe_write(&request);
            let app_id = written_app(&request);
            match council.write(request.clone()).await {
                Ok(crate::council::CouncilResponse::PolicyGenerationMoved { .. }) => {
                    policy_attempts += 1;
                    pending.push_front(request);
                    let rejudged = if policy_attempts > POLICY_ATTEMPTS {
                        Err(format!(
                            "{describe}: the cluster's policies changed {POLICY_ATTEMPTS} times \
                             during this apply; apply again"
                        ))
                    } else {
                        rejudge(
                            &council,
                            &declared,
                            &admission.defaulted,
                            &caller,
                            &mut pending,
                        )
                        .await
                    };
                    match rejudge_messages(rejudged) {
                        Ok(messages) => {
                            for message in messages {
                                let _ = event_tx.send(ApplyEvent::Progress { message }).await;
                            }
                        }
                        Err(message) => {
                            let _ = event_tx.send(ApplyEvent::Error { message }).await;
                            return;
                        }
                    }
                    continue;
                }
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
                    // Committing a spec clears a failed rollout unless it's
                    // the identical spec, which isn't retried (#929).
                    let notice = match &app_id {
                        Some(app_id) => {
                            council
                                .read_desired(|desired| rollout_failed_notice(desired, app_id))
                                .await
                        }
                        None => None,
                    };
                    if let Some(message) = notice {
                        let _ = event_tx.send(ApplyEvent::Progress { message }).await;
                    }
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

/// What `relish apply` says after committing an app whose current spec is
/// still `RolloutFailed`: the apply re-sent the identical spec, and the
/// leader doesn't retry it (#692, D11-B; #929).
fn rollout_failed_notice(
    desired: &crate::council::types::DesiredState,
    app_id: &crate::meat::AppId,
) -> Option<String> {
    let spec = desired.apps.get(app_id)?;
    let failure = desired
        .rollouts
        .get(app_id)?
        .failed_for(&crate::meat::deploy_types::rollout_fingerprint(spec))?;
    Some(format!(
        "warning: app '{}' in namespace '{}' is RolloutFailed on {} ({}); the same spec \
         again isn't retried. Change the spec, or run relish rollback",
        app_id.name, app_id.namespace, failure.node, failure.reason
    ))
}

/// How many times one apply judges its apps again after the policies moved
/// under it, before it gives up and asks the caller to apply again.
const POLICY_ATTEMPTS: usize = 3;

/// The caller a policy sees for an API request: the token, or the local
/// bootstrap admin when the API has no auth.
pub(crate) fn policy_caller(
    auth: Option<&crate::sesame::auth::AuthContext>,
) -> crate::policy::Caller {
    match auth {
        Some(auth) => crate::policy::Caller::token(&auth.token_name, auth.role),
        None => {
            crate::policy::Caller::token("local-bootstrap", crate::sesame::types::ApiRole::Admin)
        }
    }
}

/// The caller resource owners see for an API request (#1125). The
/// bootstrap window (no auth) and the service principal, which only ever
/// carries a request another node already checked, count as admins.
pub(crate) fn owner_caller(
    auth: Option<&crate::sesame::auth::AuthContext>,
) -> crate::council::owner::OwnerCaller {
    use crate::council::owner::OwnerCaller;
    match auth {
        None => OwnerCaller::token("local-bootstrap", true),
        Some(auth) if auth.token_name == crate::sesame::auth::SYSTEM_PRINCIPAL => {
            OwnerCaller::token(&auth.token_name, true)
        }
        Some(auth) => OwnerCaller::token(
            &auth.token_name,
            auth.role == crate::sesame::types::ApiRole::Admin,
        ),
    }
}

/// Take the apps a controller owns out of `config`, unless it declares
/// their owner, and return a progress note for each one skipped (#1125).
/// Refuses the whole apply when it declares an owner the caller can't
/// speak for, or one that differs from the app's current owner.
pub(crate) async fn skip_owned_apps(
    council: &crate::council::CouncilNode,
    config: &mut Config,
    caller: &crate::council::owner::OwnerCaller,
) -> Result<Vec<String>, crate::council::owner::OwnerError> {
    use crate::council::owner::{ApplyVerdict, judge_apply, skip_note};
    let declared: Vec<(String, crate::meat::AppId, Option<String>)> = config
        .app
        .iter()
        .map(|(name, spec)| {
            let app_id =
                crate::meat::AppId::new(name, spec.namespace.as_deref().unwrap_or("default"));
            (name.clone(), app_id, spec.owner.clone())
        })
        .collect();
    let current: std::collections::HashMap<crate::meat::AppId, String> = council
        .read_desired(|desired| {
            declared
                .iter()
                .filter_map(|(_, app_id, _)| {
                    let owner = desired.apps.get(app_id)?.owner.clone()?;
                    Some((app_id.clone(), owner))
                })
                .collect()
        })
        .await;
    let mut notes = Vec::new();
    for (name, app_id, owner) in &declared {
        let verdict = judge_apply(
            app_id,
            owner.as_deref(),
            current.get(app_id).map(String::as_str),
            caller,
        )?;
        if let ApplyVerdict::Skip { owner } = verdict {
            config.app.remove(name);
            notes.push(skip_note(app_id, &owner));
        }
    }
    Ok(notes)
}

/// The answer to a change resource owners refused: 403 or 409, with the
/// reason.
pub(crate) fn owner_refusal(error: &crate::council::owner::OwnerError) -> Response {
    let status = StatusCode::from_u16(error.status()).unwrap_or(StatusCode::CONFLICT);
    (
        status,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}

/// Check an imperative change (`verb`) against the app's owner, from
/// desired state. Any node can answer it: a forwarded request then arrives
/// at the leader as the service principal, which passes.
pub(crate) async fn owner_command_refusal(
    council: &crate::council::CouncilNode,
    auth: Option<&crate::sesame::auth::AuthContext>,
    app_id: &crate::meat::AppId,
    verb: &'static str,
) -> Option<Response> {
    let current = council
        .read_desired(|desired| desired.apps.get(app_id).and_then(|spec| spec.owner.clone()))
        .await;
    let caller = owner_caller(auth);
    let judged = if verb == "release" {
        crate::council::owner::judge_release(app_id, current.as_deref(), &caller)
    } else {
        crate::council::owner::judge_command(app_id, current.as_deref(), &caller, verb)
    };
    judged.err().map(|error| owner_refusal(&error))
}

/// The app an apply write commits, looking inside a policy fence.
fn written_app(request: &crate::council::RaftRequest) -> Option<crate::meat::AppId> {
    use crate::council::RaftRequest;
    match request {
        RaftRequest::AppSpec { app_id, .. } | RaftRequest::TestLeaseAppSpec { app_id, .. } => {
            Some(app_id.clone())
        }
        RaftRequest::PolicyFenced { change, .. } => written_app(change),
        _ => None,
    }
}

/// Judge the apps still in `pending` again, under the policies now in
/// force, and fence them on the new generation. Their declared specs come
/// from `declared`, the config before its defaults were filled in and its
/// images bound. The apply's own policies are committed by now, so the
/// committed set is the whole story. Returns the warnings, or why the
/// apply stops.
///
/// The pending writes already carry the specs `defaulted` filled in. When
/// the new policies would fill in a remaining app differently, the apply
/// stops rather than write a spec no policy set produced (#1118).
pub(super) async fn rejudge(
    council: &crate::council::CouncilNode,
    declared: &Config,
    defaulted: &std::collections::BTreeMap<crate::meat::AppId, crate::policy::DefaultedApp>,
    caller: &crate::policy::Caller,
    pending: &mut std::collections::VecDeque<crate::council::RaftRequest>,
) -> Result<Vec<crate::policy::Finding>, String> {
    let mut remaining = Config::default();
    for app_id in pending.iter().filter_map(written_app) {
        if let Some(spec) = declared.app.get(&app_id.name) {
            remaining.app.insert(app_id.name.clone(), spec.clone());
        }
    }
    let (policies, generation) = council
        .read_desired(|desired| (desired.policies.clone(), desired.policy_generation))
        .await;
    let admitted = crate::policy::admit_blocking(
        policies,
        generation,
        remaining,
        caller.clone(),
        crate::policy::Source::Apply,
    )
    .await
    .map_err(|error| error.to_string())?;
    for app_id in pending.iter().filter_map(written_app) {
        let before = defaulted.get(&app_id).map(|app| &app.effective);
        let now = admitted.defaulted.get(&app_id).map(|app| &app.effective);
        if before != now {
            return Err(format!(
                "the policies now fill in app {}/{} differently; apply again",
                app_id.namespace, app_id.name
            ));
        }
    }
    for write in pending.iter_mut() {
        if let crate::council::RaftRequest::PolicyFenced { generation, .. } = write {
            *generation = admitted.generation;
        }
    }
    Ok(admitted.warnings)
}

/// The progress lines a successful re-judgement streams, or the error.
fn rejudge_messages(
    rejudged: Result<Vec<crate::policy::Finding>, String>,
) -> Result<Vec<String>, String> {
    let warnings =
        rejudged.map_err(|reason| format!("refused after the policies changed: {reason}"))?;
    Ok(std::iter::once(
        "the cluster's policies changed during this apply; judged the remaining apps again"
            .to_string(),
    )
    .chain(warnings.iter().map(|finding| format!("warning: {finding}")))
    .collect())
}

/// The answer to an apply the policies refused, after auditing it: 422
/// with every denial and warning named.
pub(crate) async fn policy_refusal(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    error: &crate::policy::AdmitError,
) -> Response {
    let (denials, warnings) = match error {
        crate::policy::AdmitError::Denied(denied) => (
            denied.denials.iter().map(ToString::to_string).collect(),
            denied.warnings.iter().map(ToString::to_string).collect(),
        ),
        crate::policy::AdmitError::Broken { .. }
        | crate::policy::AdmitError::Interrupted { .. } => (vec![error.to_string()], Vec::new()),
    };
    let policies: std::collections::BTreeSet<&str> = match error {
        crate::policy::AdmitError::Denied(denied) => denied
            .denials
            .iter()
            .map(|finding| finding.policy.as_str())
            .collect(),
        crate::policy::AdmitError::Broken { name, .. } => [name.as_str()].into(),
        crate::policy::AdmitError::Interrupted { .. } => std::collections::BTreeSet::new(),
    };
    let mut details = std::collections::BTreeMap::new();
    details.insert(
        "policies".to_string(),
        policies.into_iter().collect::<Vec<_>>().join(","),
    );
    super::record_caller_action(
        state,
        auth,
        super::CallerAction {
            kind: crate::bun::events::EventKind::Operation,
            severity: crate::bun::events::EventSeverity::Warning,
            action: "policy.denied",
            app: None,
            namespace: None,
            node: None,
            details,
            message: format!("apply refused by policy: {error}"),
        },
    )
    .await;
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(PolicyRefusal {
            error: error.to_string(),
            policy_denials: denials,
            policy_warnings: warnings,
        }),
    )
        .into_response()
}

/// Record what `audit` policies found in an apply that goes ahead
/// (#1119): one `policy.audited` event naming the policies, with every
/// finding in its message. The caller isn't told; the audit log is where
/// an audit policy's findings go.
pub(crate) async fn record_policy_audits(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    audits: &[crate::policy::Finding],
) {
    if audits.is_empty() {
        return;
    }
    let policies: std::collections::BTreeSet<&str> = audits
        .iter()
        .map(|finding| finding.policy.as_str())
        .collect();
    let mut details = std::collections::BTreeMap::new();
    details.insert(
        "policies".to_string(),
        policies.into_iter().collect::<Vec<_>>().join(","),
    );
    let findings: Vec<String> = audits.iter().map(ToString::to_string).collect();
    super::record_caller_action(
        state,
        auth,
        super::CallerAction {
            kind: crate::bun::events::EventKind::Operation,
            severity: crate::bun::events::EventSeverity::Info,
            action: "policy.audited",
            app: None,
            namespace: None,
            node: None,
            details,
            message: format!("audit policies recorded: {}", findings.join("; ")),
        },
    )
    .await;
}

/// The body of a 422 the policies caused (#1117).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRefusal {
    /// Every denial, on one line.
    pub error: String,
    /// One line per denial.
    pub policy_denials: Vec<String>,
    /// One line per warning the same apply drew.
    #[serde(default)]
    pub policy_warnings: Vec<String>,
}

/// A short human-readable label for an apply progress message.
pub(super) fn describe_write(request: &crate::council::types::RaftRequest) -> String {
    use crate::council::types::RaftRequest;
    match request {
        RaftRequest::AppSpec { app_id, .. } => format!("app {}", app_id.name),
        RaftRequest::NamespaceSpec { name, .. } => format!("namespace {name}"),
        RaftRequest::PermissionSpec { name, .. } => format!("permission {name}"),
        RaftRequest::PolicySpec { name, .. } => format!("policy {name}"),
        RaftRequest::PolicyFenced { change, .. } => describe_write(change),
        RaftRequest::TestLeaseAppSpec { app_id, .. } => {
            format!("leased app {}", app_id.name)
        }
        RaftRequest::TestLeaseNamespaceSpec { name, .. } => {
            format!("leased namespace {name}")
        }
        _ => "resource".to_string(),
    }
}

/// Marks a request one node has forwarded to the leader. A node that
/// receives a marked request and can't confirm it leads answers 503 rather
/// than forwarding it again, so no forwarding chain can loop (#1069).
pub(crate) const LEADER_FORWARDED_HEADER: &str = "x-reliaburger-forwarded";

/// Why a request that only the leader can serve can't be sent on to it.
///
/// Every variant answers 503: the caller should retry, and by then a leader
/// may be known, or may have confirmed it leads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum NoLeaderRoute {
    /// No node is known to lead yet.
    #[error("no cluster leader known yet; retry shortly")]
    Unknown,
    /// Raft names this node as the leader, but it can't confirm that with a
    /// quorum: a restored leader during the startup recovery probe, or one
    /// cut off from its followers. Forwarding would send the request back to
    /// this node's own API.
    #[error("this node holds the leader role but can't confirm it with a quorum; retry shortly")]
    ThisNode,
    /// Another node already forwarded this request here, and this node
    /// can't confirm it leads either. The two disagree about who leads, and
    /// forwarding again could start a loop.
    #[error(
        "this request was already forwarded to this node, which can't confirm it leads; retry once the election settles"
    )]
    AlreadyForwarded,
}

impl IntoResponse for NoLeaderRoute {
    fn into_response(self) -> Response {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": self.to_string() })),
        )
            .into_response()
    }
}

/// Mark `request` as forwarded to the leader, so the node that receives it
/// never forwards it again.
pub(crate) fn mark_leader_forwarded(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request.header(LEADER_FORWARDED_HEADER, "1")
}

/// The Raft id of the leader to forward a request to, or `None` when no
/// leader is known yet. Refuses when the request was already forwarded once
/// (`inbound` carries [`LEADER_FORWARDED_HEADER`]) and when the leader Raft
/// names is this node.
///
/// Callers ask this only after this node failed to confirm it leads. The
/// header can come from any client, but all it ever does is turn a forward
/// into a 503: it grants nothing.
pub(crate) async fn leader_to_forward_to(
    council: &crate::council::CouncilNode,
    inbound: &HeaderMap,
) -> Result<Option<u64>, NoLeaderRoute> {
    if inbound.contains_key(LEADER_FORWARDED_HEADER) {
        return Err(NoLeaderRoute::AlreadyForwarded);
    }
    // Raft's metrics can name this node even when `current_leader` hides it
    // (a fenced node), and the gossip directory resolves the leader from
    // those metrics, so both views must rule this node out.
    let (own_id, metrics_name_self) = {
        let metrics = council.metrics();
        let metrics = metrics.borrow();
        (metrics.id, metrics.current_leader == Some(metrics.id))
    };
    match council.current_leader().await {
        _ if metrics_name_self => Err(NoLeaderRoute::ThisNode),
        Some(leader) if leader == own_id => Err(NoLeaderRoute::ThisNode),
        leader => Ok(leader),
    }
}

/// Resolve the current leader's API base URL, for a request this node
/// can't serve because it can't confirm it leads.
///
/// It never names this node: a leader that can't prove it leads gets
/// [`NoLeaderRoute::ThisNode`], and a request another node already
/// forwarded gets [`NoLeaderRoute::AlreadyForwarded`] (see
/// [`leader_to_forward_to`]). `inbound` is the request being forwarded;
/// a request this node starts itself passes an empty map.
///
/// Preferred source is the gossip-fed membership table (it stores real
/// per-node API addresses); the fallback derives from the leader's
/// raft IP and this node's own API port, which is correct only when
/// ports are uniform across the cluster.
pub(crate) async fn leader_api_url(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    inbound: &HeaderMap,
) -> Result<String, NoLeaderRoute> {
    let leader_id = leader_to_forward_to(council, inbound)
        .await?
        .ok_or(NoLeaderRoute::Unknown)?;
    let (leader_name, leader_ip) = {
        let metrics = council.metrics();
        let metrics = metrics.borrow();
        let info = metrics
            .membership_config
            .membership()
            .get_node(&leader_id)
            .ok_or(NoLeaderRoute::Unknown)?;
        (info.name.clone(), info.addr.ip())
    };

    if let Some(membership) = &state.membership {
        let members = membership.read().await;
        if let Some(info) = members
            .iter()
            .find(|m| m.node_id == crate::meat::NodeId::new(&leader_name))
        {
            return Ok(state.cluster_http.url(&info.address.to_string(), ""));
        }
    }

    Ok(state
        .cluster_http
        .url(&format!("{leader_ip}:{}", state.api_port), ""))
}
