//! Test lease routes: create, read, renew, release and retire leases for
//! `relish test` runs, forwarded to the council leader.

use super::*;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateTestLeaseRequest {
    #[serde(default)]
    pub(super) scope: LeaseScope,
    pub(super) ttl_seconds: u64,
    pub(super) namespace: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RenewTestLeaseRequest {
    pub(super) ttl_seconds: u64,
}

#[allow(clippy::result_large_err)]
pub(super) fn authenticated_test_user(
    auth: Option<&crate::sesame::auth::AuthContext>,
    required: crate::sesame::types::ApiRole,
) -> Result<&crate::sesame::auth::AuthContext, Response> {
    let Some(auth) = auth else {
        return Err((StatusCode::UNAUTHORIZED, "authentication required").into_response());
    };
    crate::sesame::auth::authorize_user(Some(auth), required)?;
    Ok(auth)
}

#[allow(clippy::result_large_err)]
pub(super) fn test_operation_authorisation(
    state: &ApiState,
    auth: &crate::sesame::auth::AuthContext,
) -> Result<(), Response> {
    state
        .static_capabilities
        .test_policy
        .authorise(
            crate::testkit::safety::OperationPermission::ProvisionIsolatedWorkloads,
            &crate::testkit::safety::OperationAuthorisation {
                principal: &auth.token_name,
                role: auth.role,
                acknowledged: false,
            },
        )
        .map(|_| ())
        .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()).into_response())
}

#[allow(clippy::result_large_err)]
pub(super) fn validate_lease_ttl(state: &ApiState, ttl_seconds: u64) -> Result<u64, Response> {
    if ttl_seconds == 0 || ttl_seconds > state.static_capabilities.test_policy.max_lease_seconds {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!(
                    "ttl_seconds must be between 1 and {}",
                    state.static_capabilities.test_policy.max_lease_seconds
                )
            })),
        )
            .into_response());
    }
    Ok(ttl_seconds.saturating_mul(1_000))
}

pub(super) async fn test_lease_create_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<CreateTestLeaseRequest>,
) -> Response {
    let auth =
        match authenticated_test_user(auth.as_deref(), crate::sesame::types::ApiRole::Deployer) {
            Ok(auth) => auth,
            Err(response) => return response,
        };
    // A lease provisions cluster resources for tests, so under a
    // `[permission]` block it needs `admin` across the cluster (D5).
    if let Err(response) =
        enforce_cluster_permission(&state, Some(auth), crate::config::PermissionAction::Admin).await
    {
        return response;
    }
    if let Err(response) = test_operation_authorisation(&state, auth) {
        return response;
    }
    let ttl_millis = match validate_lease_ttl(&state, request.ttl_seconds) {
        Ok(ttl) => ttl,
        Err(response) => return response,
    };
    if request.scope == LeaseScope::NodeJobs {
        if let Err(response) = crate::sesame::auth::require_unscoped(Some(auth)) {
            return response;
        }
        if request.namespace.is_some() {
            return lease_error_response(crate::testkit::lease::LeaseError::InvalidScope);
        }
    }
    if let Some(council) = &state.council
        && request.scope == LeaseScope::Applications
        && !council.is_leader().await
    {
        let created = forward_test_lease_request(
            &state,
            council,
            reqwest::Method::POST,
            "/v1/test/leases",
            &headers,
            Some(&request),
        )
        .await;
        return await_forwarded_lease_replica(council, created).await;
    }
    let mut random = [0u8; 16];
    if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut random).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to generate lease id",
        )
            .into_response();
    }
    let random_id = hex::encode(random);
    let (lease_id, namespace) = match request.scope {
        LeaseScope::Applications => {
            let namespace = request
                .namespace
                .unwrap_or_else(|| format!("rbtest-{}", &random_id[..12]));
            (random_id, namespace)
        }
        LeaseScope::NodeJobs => (
            format!("node-jobs-{random_id}"),
            format!("rbtest-node-{random_id}"),
        ),
    };
    if !crate::testkit::lease::valid_test_namespace(&namespace) {
        return (
            StatusCode::BAD_REQUEST,
            crate::testkit::lease::LeaseError::InvalidNamespace.to_string(),
        )
            .into_response();
    }
    if auth
        .scoped_namespaces
        .as_ref()
        .is_some_and(|namespaces| !namespaces.contains(&namespace))
    {
        return (
            StatusCode::FORBIDDEN,
            "token scope does not allow the requested test namespace",
        )
            .into_response();
    }
    let now = crate::testkit::lease::now_unix_millis();
    let lease = match crate::testkit::lease::TestLease::new_scoped(
        lease_id,
        auth.principal_id.clone(),
        auth.token_name.clone(),
        namespace,
        now,
        now.saturating_add(ttl_millis),
        request.scope,
    ) {
        Ok(lease) => lease,
        Err(error) => return lease_error_response(error),
    };

    if let Some(council) = &state.council
        && request.scope == LeaseScope::Applications
    {
        if let Err(response) = write_lease_request(
            council,
            crate::council::RaftRequest::TestLeaseCreate(lease.clone()),
        )
        .await
        {
            return response;
        }
    } else if let Err(error) = state.local_test_leases.create(lease.clone()).await {
        return lease_error_response(error);
    }
    (StatusCode::CREATED, Json(lease)).into_response()
}

/// How long a follower holds a forwarded lease creation for its own replica.
pub(super) const FORWARDED_LEASE_REPLICA_WAIT: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Hold a follower's forwarded lease creation until its own replica has it.
///
/// The leader answers once a quorum has committed the lease, and that quorum
/// need not include this follower. The caller's next request, an apply that
/// carries the lease, usually comes back to this node, which checks the lease
/// against its local replica before forwarding the apply. Answering early let
/// that check report "lease not found" for a lease the caller had just been
/// given. A replica still behind at the deadline gets the lease returned
/// anyway: it exists, and a later request will find it.
pub(super) async fn await_forwarded_lease_replica(
    council: &crate::council::CouncilNode,
    created: Response,
) -> Response {
    if created.status() != StatusCode::CREATED {
        return created;
    }
    let (parts, body) = created.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_LEASE_FORWARD_RESPONSE_BYTES).await else {
        return (
            StatusCode::BAD_GATEWAY,
            "failed to read leader lease response",
        )
            .into_response();
    };
    if let Ok(lease) = serde_json::from_slice::<crate::testkit::lease::TestLease>(&bytes) {
        // Subscribe before the first look, so an entry applied between the
        // look and the wait still wakes it.
        let mut applied = council.metrics();
        let _ = tokio::time::timeout(FORWARDED_LEASE_REPLICA_WAIT, async {
            while !council
                .desired_state()
                .await
                .test_leases
                .contains_key(&lease.lease_id)
            {
                if applied.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    }
    Response::from_parts(parts, axum::body::Body::from(bytes))
}

pub(super) async fn test_lease_get_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(lease_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let auth =
        match authenticated_test_user(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly) {
            Ok(auth) => auth,
            Err(response) => return response,
        };
    if let Some(council) = &state.council
        && !is_node_job_lease(&lease_id)
        && !confirmed_lease_leader(council).await
    {
        return forward_test_lease_request::<()>(
            &state,
            council,
            reqwest::Method::GET,
            &format!("/v1/test/leases/{lease_id}"),
            &headers,
            None,
        )
        .await;
    }
    let Some(lease) = find_test_lease(&state, &lease_id).await else {
        return lease_error_response(crate::testkit::lease::LeaseError::NotFound);
    };
    if lease.owner_id != auth.principal_id {
        if auth.role != crate::sesame::types::ApiRole::Admin {
            return lease_error_response(crate::testkit::lease::LeaseError::WrongOwner);
        }
        if let Err(response) = crate::sesame::auth::require_unscoped(Some(auth)) {
            return response;
        }
        if let Err(response) =
            enforce_cluster_permission(&state, Some(auth), crate::config::PermissionAction::Admin)
                .await
        {
            return response;
        }
    }
    Json(lease).into_response()
}

pub(super) async fn test_lease_renew_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(lease_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<RenewTestLeaseRequest>,
) -> Response {
    let auth =
        match authenticated_test_user(auth.as_deref(), crate::sesame::types::ApiRole::Deployer) {
            Ok(auth) => auth,
            Err(response) => return response,
        };
    // A lease provisions cluster resources for tests, so under a
    // `[permission]` block it needs `admin` across the cluster (D5).
    if let Err(response) =
        enforce_cluster_permission(&state, Some(auth), crate::config::PermissionAction::Admin).await
    {
        return response;
    }
    if let Err(response) = test_operation_authorisation(&state, auth) {
        return response;
    }
    let ttl_millis = match validate_lease_ttl(&state, request.ttl_seconds) {
        Ok(ttl) => ttl,
        Err(response) => return response,
    };
    if let Some(council) = &state.council
        && !is_node_job_lease(&lease_id)
        && !council.is_leader().await
    {
        return forward_test_lease_request(
            &state,
            council,
            reqwest::Method::POST,
            &format!("/v1/test/leases/{lease_id}/renew"),
            &headers,
            Some(&request),
        )
        .await;
    }
    let now = crate::testkit::lease::now_unix_millis();
    let expires = now.saturating_add(ttl_millis);
    let Some(existing) = find_test_lease(&state, &lease_id).await else {
        return lease_error_response(crate::testkit::lease::LeaseError::NotFound);
    };
    if let Err(error) = existing.authorise_owner(&auth.principal_id, now) {
        return lease_error_response(error);
    }

    if let Some(council) = &state.council
        && !is_node_job_lease(&lease_id)
    {
        if let Err(response) = write_lease_request(
            council,
            crate::council::RaftRequest::TestLeaseRenew {
                lease_id: lease_id.clone(),
                owner_id: auth.principal_id.clone(),
                renewed_at_unix_ms: now,
                expires_at_unix_ms: expires,
            },
        )
        .await
        {
            return response;
        }
        let Some(lease) = find_test_lease(&state, &lease_id).await else {
            return lease_error_response(crate::testkit::lease::LeaseError::NotFound);
        };
        Json(lease).into_response()
    } else {
        match state
            .local_test_leases
            .renew(&lease_id, &auth.principal_id, now, expires)
            .await
        {
            Ok(lease) => Json(lease).into_response(),
            Err(error) => lease_error_response(error),
        }
    }
}

pub(super) async fn test_lease_release_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(lease_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let auth =
        match authenticated_test_user(auth.as_deref(), crate::sesame::types::ApiRole::Deployer) {
            Ok(auth) => auth,
            Err(response) => return response,
        };
    // A lease provisions cluster resources for tests, so under a
    // `[permission]` block it needs `admin` across the cluster (D5).
    if let Err(response) =
        enforce_cluster_permission(&state, Some(auth), crate::config::PermissionAction::Admin).await
    {
        return response;
    }
    if let Some(council) = &state.council
        && !is_node_job_lease(&lease_id)
        && !council.is_leader().await
    {
        return forward_test_lease_request::<()>(
            &state,
            council,
            reqwest::Method::DELETE,
            &format!("/v1/test/leases/{lease_id}"),
            &headers,
            None,
        )
        .await;
    }
    let Some(lease) = find_test_lease(&state, &lease_id).await else {
        return lease_error_response(crate::testkit::lease::LeaseError::NotFound);
    };
    let owner_id = if lease.owner_id == auth.principal_id {
        Some(auth.principal_id.as_str())
    } else if auth.role == crate::sesame::types::ApiRole::Admin {
        if let Err(response) = crate::sesame::auth::require_unscoped(Some(auth)) {
            return response;
        }
        if let Err(response) =
            enforce_cluster_permission(&state, Some(auth), crate::config::PermissionAction::Admin)
                .await
        {
            return response;
        }
        None
    } else {
        return lease_error_response(crate::testkit::lease::LeaseError::WrongOwner);
    };
    let result = if let Some(council) = &state.council
        && !is_node_job_lease(&lease_id)
    {
        crate::testkit::lease::cleanup_cluster_lease(council, &lease_id, owner_id).await
    } else {
        crate::testkit::lease::cleanup_local_lease(
            &state.local_test_leases,
            &state.cmd_tx,
            &lease_id,
            owner_id,
        )
        .await
    };
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(crate::testkit::lease::LeaseError::NotFound) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => lease_error_response(error),
    }
}

pub(super) async fn test_lease_retired_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Json(retirement): Json<crate::cluster::orchestrate::LeaseRetirement>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "not running in cluster mode",
        )
            .into_response();
    };
    if !confirmed_lease_leader(council).await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "retirement requires a current leader",
        )
            .into_response();
    }
    match write_lease_request(
        council,
        crate::council::RaftRequest::TestLeasePlacementRetired {
            lease_id: retirement.lease_id,
            placement: retirement.placement,
        },
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(response) => response,
    }
}

pub(super) const MAX_LEASE_FORWARD_RESPONSE_BYTES: usize = 64 * 1024;

/// Forward a lease mutation to the current leader while retaining the
/// caller's own credentials. The leader repeats authentication and policy
/// checks; a follower never replaces user authority with the service token.
pub(super) async fn forward_test_lease_request<T: Serialize + ?Sized>(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    method: reqwest::Method,
    path: &str,
    headers: &HeaderMap,
    body: Option<&T>,
) -> Response {
    let points_to_self = {
        let metrics = council.metrics();
        let metrics = metrics.borrow();
        metrics.current_leader == Some(metrics.id)
    };
    if points_to_self || headers.contains_key("x-reliaburger-lease-forwarded") {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "lease leader is unavailable; retry shortly",
        )
            .into_response();
    }
    let Some(leader_url) = leader_api_url(state, council).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no cluster leader known yet; retry shortly",
        )
            .into_response();
    };
    let mut request = state
        .cluster_http
        .client()
        .request(method, format!("{leader_url}{path}"))
        .header("x-reliaburger-lease-forwarded", "1");
    for name in [
        axum::http::header::AUTHORIZATION,
        axum::http::header::COOKIE,
    ] {
        if let Some(value) = headers.get(&name) {
            request = request.header(name.as_str(), value.as_bytes());
        }
    }
    if let Some(body) = body {
        request = request.json(body);
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut upstream = match tokio::time::timeout_at(deadline, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("failed to forward lease request to the leader: {error}"),
            )
                .into_response();
        }
        Err(_) => {
            return (StatusCode::GATEWAY_TIMEOUT, "leader request timed out").into_response();
        }
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .cloned();
    let mut bytes = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, upstream.chunk()).await {
            Ok(Ok(Some(chunk)))
                if bytes.len().saturating_add(chunk.len()) <= MAX_LEASE_FORWARD_RESPONSE_BYTES =>
            {
                bytes.extend_from_slice(&chunk);
            }
            Ok(Ok(Some(_))) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    "leader lease response exceeded 64 KiB",
                )
                    .into_response();
            }
            Ok(Ok(None)) => break,
            Ok(Err(error)) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("failed to read leader lease response: {error}"),
                )
                    .into_response();
            }
            Err(_) => {
                return (StatusCode::GATEWAY_TIMEOUT, "leader response timed out").into_response();
            }
        }
    }
    let mut response = Response::builder().status(status);
    if let Some(content_type) = content_type {
        response = response.header(axum::http::header::CONTENT_TYPE, content_type.as_bytes());
    }
    response
        .body(axum::body::Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

pub(super) async fn find_test_lease(
    state: &ApiState,
    lease_id: &str,
) -> Option<crate::testkit::lease::TestLease> {
    if is_node_job_lease(lease_id) {
        return state.local_test_leases.get(lease_id).await;
    }
    match &state.council {
        Some(council) => council
            .desired_state()
            .await
            .test_leases
            .get(lease_id)
            .cloned(),
        None => state.local_test_leases.get(lease_id).await,
    }
}

// `Response` is large but it IS the HTTP reply to send on failure —
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
pub(super) async fn write_lease_request(
    council: &crate::council::CouncilNode,
    request: crate::council::RaftRequest,
) -> Result<(), Response> {
    match council.write(request).await {
        Ok(crate::council::CouncilResponse::Refused { reason }) => {
            Err((StatusCode::CONFLICT, reason).into_response())
        }
        Ok(_) => Ok(()),
        Err(error) => Err((StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()),
    }
}

pub(super) fn lease_error_response(error: crate::testkit::lease::LeaseError) -> Response {
    let status = match error {
        crate::testkit::lease::LeaseError::NotFound => StatusCode::NOT_FOUND,
        crate::testkit::lease::LeaseError::CleanupPending => StatusCode::ACCEPTED,
        crate::testkit::lease::LeaseError::WrongOwner
        | crate::testkit::lease::LeaseError::ImageOwnership => StatusCode::FORBIDDEN,
        crate::testkit::lease::LeaseError::NotActive
        | crate::testkit::lease::LeaseError::Busy
        | crate::testkit::lease::LeaseError::AlreadyExists
        | crate::testkit::lease::LeaseError::NamespaceOwned
        | crate::testkit::lease::LeaseError::NamespaceMismatch
        | crate::testkit::lease::LeaseError::ResourceLimit => StatusCode::CONFLICT,
        crate::testkit::lease::LeaseError::TooManyLeases => StatusCode::TOO_MANY_REQUESTS,
        crate::testkit::lease::LeaseError::InvalidId
        | crate::testkit::lease::LeaseError::InvalidScope
        | crate::testkit::lease::LeaseError::InvalidOwner
        | crate::testkit::lease::LeaseError::InvalidNamespace
        | crate::testkit::lease::LeaseError::InvalidExpiry
        | crate::testkit::lease::LeaseError::InvalidToken
        | crate::testkit::lease::LeaseError::UnsupportedSchema { .. } => StatusCode::BAD_REQUEST,
        crate::testkit::lease::LeaseError::Persistence(_)
        | crate::testkit::lease::LeaseError::PersistenceUncertain
        | crate::testkit::lease::LeaseError::Malformed(_)
        | crate::testkit::lease::LeaseError::StoreTooLarge
        | crate::testkit::lease::LeaseError::Cleanup(_)
        | crate::testkit::lease::LeaseError::Consensus(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}
