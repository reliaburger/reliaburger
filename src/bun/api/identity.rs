//! Identity routes: the workload-identity JWKS and signer, API tokens
//! and join tokens.

use super::*;

/// JWKS endpoint — publishes the OIDC Ed25519 public key for JWT verification.
pub(super) async fn identity_jwks_handler(State(state): State<ApiState>) -> Response {
    let Some(ref council) = state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council available" })),
        )
            .into_response();
    };

    let security_state = council.security_state().await;
    let Some(ref oidc_config) = security_state.oidc_signing_config else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no OIDC signing config" })),
        )
            .into_response();
    };

    Json(crate::sesame::oidc::jwks_response(oidc_config)).into_response()
}

/// Attach an operator's detached image signature (from `relish sign`) to a
/// manifest via Raft. The body is a [`crate::pickle::signing::SignatureSubmission`];
/// the private key never reaches the cluster.
pub(super) async fn identity_sign_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    // AUTH4: a user-management route. The service principal must not sign
    // images, even though it can present the cluster token.
    if let Err(resp) =
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
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
    let submission: crate::pickle::signing::SignatureSubmission = match serde_json::from_str(&body)
    {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid JSON: {e}") })),
            )
                .into_response();
        }
    };

    match ask_agent(&state.cmd_tx, |response| AgentCommand::SignImage {
        submission,
        response,
    })
    .await
    {
        Ok(Ok(msg)) => Json(serde_json::json!({ "message": msg })).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent channel closed" })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Token management endpoints
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub(super) struct TokenListQuery {
    /// Answer from this node only, with this node's last-use times. Set on
    /// the fan-out's own requests so a peer never fans out again.
    #[serde(default)]
    local: bool,
}

/// List the cluster's API tokens: name, role, scope, times and when each was
/// last used. Never a secret or a hash.
///
/// The tokens come from Raft, the same on every node. Last use doesn't: each
/// node only knows the requests it authenticated, so the node asked merges
/// its own times with every live member's (the latest wins) and names any
/// member that didn't answer.
pub(super) async fn token_list_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Query(query): Query<TokenListQuery>,
) -> Response {
    // A peer's share is asked for by the node fan-out, which presents the
    // service token. That principal stays off user management (AUTH4), so
    // it may read only this node's answer, never start a cluster-wide one.
    let system_reads_local = query.local
        && auth.as_deref().is_some_and(|ctx| {
            ctx.token_name == crate::sesame::auth::SYSTEM_PRINCIPAL
                && ctx.principal_id == crate::sesame::auth::SYSTEM_PRINCIPAL
        });
    if !system_reads_local {
        if let Err(resp) = crate::sesame::auth::authorize_user(
            auth.as_deref(),
            crate::sesame::types::ApiRole::Admin,
        ) {
            return resp;
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
    let Some(ref council) = state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council available" })),
        )
            .into_response();
    };

    let security_state = council.security_state().await;
    let last_used = state.token_last_used.read().await.clone();
    let unix_seconds = |at: std::time::SystemTime| {
        at.duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    };
    let tokens: Vec<crate::bun::cluster_view::TokenSummary> = security_state
        .api_tokens
        .iter()
        .map(|token| {
            let principal = crate::sesame::auth::token_principal_id(token);
            crate::bun::cluster_view::TokenSummary {
                name: token.name.clone(),
                role: token.role.to_string(),
                scope: token.scope.clone(),
                created_at: unix_seconds(token.created_at),
                expires_at: token.expires_at.map(unix_seconds),
                last_used: last_used.get(&principal).copied(),
                principal,
            }
        })
        .collect();

    let mut warnings = Vec::new();
    let tokens = if query.local {
        tokens
    } else {
        let (peers, failures) = fan_out_to_peers::<crate::bun::cluster_view::ClusterTokens>(
            &state,
            "/v1/token/list?local=true",
            CLUSTER_STATUS_TIMEOUT,
        )
        .await;
        warnings = failures;
        crate::bun::cluster_view::merge_token_last_used(
            tokens,
            peers.into_iter().map(|(_, view)| view.tokens).collect(),
        )
    };

    Json(crate::bun::cluster_view::ClusterTokens { tokens, warnings }).into_response()
}

/// Revoke an API token by name via Raft.
pub(super) async fn token_revoke_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
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
    #[derive(serde::Deserialize)]
    struct RevokeRequest {
        name: String,
    }

    let req: RevokeRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid JSON: {e}") })),
            )
                .into_response();
        }
    };

    let Some(ref council) = state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council available" })),
        )
            .into_response();
    };

    match council
        .write(crate::council::RaftRequest::RevokeApiToken {
            name: req.name.clone(),
        })
        .await
    {
        Ok(crate::council::CouncilResponse::Refused { reason }) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": reason })),
        )
            .into_response(),
        Ok(_) => {
            record_caller_audit(
                &state,
                auth.as_deref(),
                crate::bun::events::EventKind::Token,
                "token.revoked",
                std::collections::BTreeMap::from([("token".to_string(), req.name.clone())]),
                format!("API token {} revoked", req.name),
            )
            .await;
            Json(serde_json::json!({ "message": format!("token {} revoked", req.name) }))
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Create an API token and persist it via Raft.
///
/// The token is minted server-side (Argon2 hashing) and written to the
/// SecurityState in one step, so the stored hash always matches the plaintext
/// returned to the caller. The plaintext is shown once and never stored.
pub(super) async fn token_create_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // AUTH4: the highest-value lateral-movement target. A stolen service
    // token must not be able to mint fresh user tokens.
    if let Err(resp) =
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
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
    #[derive(serde::Deserialize, serde::Serialize)]
    struct CreateRequest {
        name: String,
        role: String,
        #[serde(default)]
        apps: Option<Vec<String>>,
        #[serde(default)]
        namespaces: Option<Vec<String>>,
        #[serde(default)]
        ttl_days: Option<u64>,
        /// No expiry, overriding the node's default lifetime.
        #[serde(default)]
        no_expiry: bool,
        /// Take over the `[permission]` spec already keyed by this name.
        #[serde(default)]
        inherit_permissions: bool,
        #[serde(default)]
        lease_id: Option<String>,
    }

    let req: CreateRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid JSON: {e}") })),
            )
                .into_response();
        }
    };

    // The internal service principal name is reserved: a user token minted with
    // it would match `SYSTEM_PRINCIPAL` and bypass scope confinement (AUTH4).
    if req.name == crate::sesame::auth::SYSTEM_PRINCIPAL {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("token name {:?} is reserved", req.name)
            })),
        )
            .into_response();
    }

    let role = match req.role.as_str() {
        "admin" => crate::sesame::types::ApiRole::Admin,
        "deployer" => crate::sesame::types::ApiRole::Deployer,
        "read-only" | "readonly" => crate::sesame::types::ApiRole::ReadOnly,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("unknown role: {other} (expected admin, deployer, or read-only)")
                })),
            )
                .into_response();
        }
    };

    let Some(ref council) = state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council available" })),
        )
            .into_response();
    };

    let lease_owner = if req.lease_id.is_some() {
        let user =
            match authenticated_test_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin) {
                Ok(user) => user,
                Err(response) => return response,
            };
        if let Err(response) = crate::sesame::auth::require_unscoped(Some(user)) {
            return response;
        }
        if let Err(response) = test_operation_authorisation(&state, user) {
            return response;
        }
        if !council.is_leader().await {
            return forward_test_lease_request(
                &state,
                council,
                reqwest::Method::POST,
                "/v1/token/create",
                &headers,
                Some(&req),
            )
            .await;
        }
        Some(user.principal_id.clone())
    } else {
        None
    };
    let lease = if let Some(lease_id) = &req.lease_id {
        let Some(lease) = find_test_lease(&state, lease_id).await else {
            return lease_error_response(crate::testkit::lease::LeaseError::NotFound);
        };
        if let Err(error) = lease.authorise_owner(
            lease_owner.as_deref().unwrap_or_default(),
            crate::testkit::lease::now_unix_millis(),
        ) {
            return lease_error_response(error);
        }
        Some(lease)
    } else {
        None
    };

    // Decision 3 (F05): `[permission]` specs are keyed by name, so a token
    // created under a name that has one would silently inherit it, as a
    // re-created token used to inherit its revoked namesake's. Inheriting
    // has to be asked for.
    if !req.inherit_permissions
        && council
            .desired_state()
            .await
            .permissions
            .contains_key(&req.name)
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "a [permission.{name}] spec exists, and a token named {name:?} would \
                     inherit it; pass --inherit-permissions to accept that, or remove or \
                     re-apply the spec first",
                    name = req.name
                )
            })),
        )
            .into_response();
    }

    let scope = crate::sesame::types::TokenScope {
        apps: req.apps.clone(),
        namespaces: req.namespaces.clone(),
    };
    let mut expires_at = match req.ttl_days {
        Some(_) if req.no_expiry => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "ttl_days and no_expiry can't both be given"
                })),
            )
                .into_response();
        }
        None if req.no_expiry => None,
        // No lifetime asked for: this node's `[security.tokens]` default,
        // which Admin tokens are exempt from.
        None => state
            .static_capabilities
            .token_lifetime
            .default_expiry(role, std::time::SystemTime::now()),
        Some(days) => {
            let expiry = days
                .checked_mul(86_400)
                .filter(|seconds| *seconds > 0)
                .and_then(|seconds| {
                    std::time::SystemTime::now()
                        .checked_add(std::time::Duration::from_secs(seconds))
                });
            let Some(expiry) = expiry else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "ttl_days must be positive and produce a representable expiry"
                    })),
                )
                    .into_response();
            };
            Some(expiry)
        }
    };

    if let Some(lease) = &lease {
        let Some(bound) = std::time::UNIX_EPOCH
            .checked_add(std::time::Duration::from_millis(lease.expires_at_unix_ms))
        else {
            return lease_error_response(crate::testkit::lease::LeaseError::InvalidExpiry);
        };
        expires_at = Some(expires_at.map_or(bound, |expiry| expiry.min(bound)));
    }

    // Argon2id hashing is deliberately slow + memory-hungry (M7): run it on the
    // blocking pool so it doesn't stall the async runtime worker.
    let name = req.name.clone();
    let expires_unix = expires_at.map(unix_seconds);
    let created = match tokio::task::spawn_blocking(move || {
        crate::sesame::token::create_token(&name, role, scope, expires_at)
    })
    .await
    {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "token hashing task failed" })),
            )
                .into_response();
        }
    };

    let request = match (lease, lease_owner) {
        (Some(lease), Some(owner_id)) => crate::council::RaftRequest::TestLeaseApiToken {
            lease_id: lease.lease_id,
            owner_id,
            observed_at_unix_ms: crate::testkit::lease::now_unix_millis(),
            token: Box::new(created.token),
        },
        _ => crate::council::RaftRequest::CreateApiToken(created.token),
    };
    if let Err(response) = write_lease_request(council, request).await {
        return response;
    }
    let mut details = std::collections::BTreeMap::from([
        ("token".to_string(), req.name.clone()),
        ("role".to_string(), req.role.clone()),
    ]);
    if let Some(at) = expires_unix {
        details.insert("expires_at".to_string(), at.to_string());
    }
    if let Some(apps) = &req.apps {
        details.insert("apps".to_string(), apps.join(","));
    }
    if let Some(namespaces) = &req.namespaces {
        details.insert("namespaces".to_string(), namespaces.join(","));
    }
    record_caller_audit(
        &state,
        auth.as_deref(),
        crate::bun::events::EventKind::Token,
        "token.created",
        details,
        format!("API token {} created with role {}", req.name, req.role),
    )
    .await;
    Json(serde_json::json!({
        "name": req.name,
        "role": req.role,
        "token": created.plaintext,
        "expires_at": expires_unix,
    }))
    .into_response()
}

/// Seconds since the Unix epoch; zero for a time before it.
fn unix_seconds(at: std::time::SystemTime) -> u64 {
    at.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Install a committed rotation in this node's auth store now, so the new
/// secret works on the next request here instead of after the next periodic
/// refresh from Raft (every 5 seconds). Other nodes pick it up on theirs.
async fn apply_rotation_locally(state: &ApiState, rotation: &crate::sesame::types::TokenRotation) {
    let Some(store) = &state.token_store else {
        return;
    };
    // The write lock covers one in-memory update; it's never held across I/O.
    let mut tokens = store.write().await;
    // Skip a token the periodic refresh already brought up to date, or the
    // new secret would be installed over itself and the old one lost.
    if let Some(token) = tokens
        .iter_mut()
        .find(|token| token.name == rotation.name && token.token_hash != rotation.token_hash)
    {
        crate::sesame::token::apply_rotation(token, rotation);
    }
}

/// Give an API token a new secret under the same name (F05 I3).
///
/// The new secret works at once. The old one keeps working for the grace
/// period (24 hours unless `grace_hours` says otherwise; 0 ends it now, for
/// a leaked secret), so clients can move over without an outage. The old
/// secret is its own principal, so browser sessions opened with it end with
/// it. The token's role, scope and `[permission]` spec stay with the name.
/// Rotating the last Admin is fine: the store keeps the same Admin.
pub(super) async fn token_rotate_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
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
    #[derive(serde::Deserialize)]
    struct RotateRequest {
        name: String,
        #[serde(default)]
        grace_hours: Option<u64>,
    }

    let req: RotateRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid JSON: {e}") })),
            )
                .into_response();
        }
    };
    let grace = match req.grace_hours {
        None => crate::sesame::token::DEFAULT_ROTATION_GRACE,
        Some(hours) => match hours.checked_mul(3_600) {
            Some(seconds) => std::time::Duration::from_secs(seconds),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "grace_hours is too large" })),
                )
                    .into_response();
            }
        },
    };

    let Some(ref council) = state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council available" })),
        )
            .into_response();
    };
    let Some(stored) = council
        .security_state()
        .await
        .api_tokens
        .into_iter()
        .find(|token| token.name == req.name)
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no API token named {:?}", req.name) })),
        )
            .into_response();
    };

    // Argon2id hashing runs on the blocking pool (M7), as in create.
    let rotated = match tokio::task::spawn_blocking(move || {
        crate::sesame::token::rotate_token(&stored, std::time::SystemTime::now(), grace)
    })
    .await
    {
        Ok(Ok(rotated)) => rotated,
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "token hashing task failed" })),
            )
                .into_response();
        }
    };
    let expires_at = rotated.rotation.expires_at.map(unix_seconds);
    let previous_valid_until = rotated.rotation.previous_valid_until.map(unix_seconds);

    match council
        .write(crate::council::RaftRequest::RotateApiToken(
            rotated.rotation.clone(),
        ))
        .await
    {
        Ok(crate::council::CouncilResponse::Refused { reason }) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": reason })),
            )
                .into_response();
        }
        Ok(_) => {}
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }
    apply_rotation_locally(&state, &rotated.rotation).await;

    let mut details = std::collections::BTreeMap::from([("token".to_string(), req.name.clone())]);
    let summary = match previous_valid_until {
        Some(until) => {
            details.insert("previous_valid_until".to_string(), until.to_string());
            format!(
                "API token {} rotated; the old secret works until {until}",
                req.name
            )
        }
        None => {
            details.insert("previous_valid_until".to_string(), "now".to_string());
            format!(
                "API token {} rotated; the old secret stopped working at once",
                req.name
            )
        }
    };
    record_caller_audit(
        &state,
        auth.as_deref(),
        crate::bun::events::EventKind::Token,
        "token.rotated",
        details,
        summary,
    )
    .await;
    Json(serde_json::json!({
        "name": req.name,
        "token": rotated.plaintext,
        "expires_at": expires_at,
        "previous_valid_until": previous_valid_until,
    }))
    .into_response()
}

/// Create a short-lived, single-use node join token and persist its hash.
///
/// This is deliberately separate from API bearer-token management. The
/// plaintext exists only in this request and response; Raft receives the
/// SHA-256 hash, expiry and attestation policy.
pub(super) async fn join_token_create_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
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

    fn default_ttl_seconds() -> u64 {
        crate::sesame::join::DEFAULT_JOIN_TOKEN_TTL.as_secs()
    }

    #[derive(serde::Deserialize)]
    struct CreateRequest {
        #[serde(default = "default_ttl_seconds")]
        ttl_seconds: u64,
        /// The node id this token may enrol (M4). Required: a token is bound to
        /// exactly one node id so it cannot be replayed to impersonate another.
        #[serde(default)]
        node_id: String,
    }

    let req: CreateRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid JSON: {e}") })),
            )
                .into_response();
        }
    };

    let Some(ref council) = state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council available" })),
        )
            .into_response();
    };

    let ttl = std::time::Duration::from_secs(req.ttl_seconds);
    let (plaintext, join_token) = match crate::sesame::join::create_join_token(ttl, &req.node_id) {
        Ok(created) => created,
        Err(crate::sesame::join::JoinError::EmptyNodeId) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "node_id is required" })),
            )
                .into_response();
        }
        Err(crate::sesame::join::JoinError::InvalidTtl { .. }) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!(
                        "ttl_seconds must be between {} and {}",
                        crate::sesame::join::MIN_JOIN_TOKEN_TTL.as_secs(),
                        crate::sesame::join::MAX_JOIN_TOKEN_TTL.as_secs(),
                    )
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let expires_at = join_token
        .expires_at
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    match council
        .write(crate::council::RaftRequest::CreateJoinToken(join_token))
        .await
    {
        Ok(_) => {
            record_caller_audit(
                &state,
                auth.as_deref(),
                crate::bun::events::EventKind::Token,
                "join_token.created",
                std::collections::BTreeMap::from([
                    ("node_id".to_string(), req.node_id.clone()),
                    ("ttl_seconds".to_string(), req.ttl_seconds.to_string()),
                ]),
                format!("join token created for node {}", req.node_id),
            )
            .await;
            Json(serde_json::json!({
                "token": plaintext,
                "ttl_seconds": req.ttl_seconds,
                "expires_at": expires_at,
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": format!("failed to commit join token (try the cluster leader): {e}")
            })),
        )
            .into_response(),
    }
}
