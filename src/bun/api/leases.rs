//! The agent lease routes (#1686): list leases, and take, renew, release or
//! break one.
//!
//! Every write forwards to the leader, which stamps its own clock into the
//! Raft entry, so every replica judges expiry against the same instant. The
//! handlers record their own audit records, because the action depends on
//! the outcome: taking a lease you already hold is `lease.renewed`, and a
//! lease someone else holds is `lease.refused`, naming them.
//!
//! The target travels in the JSON body, `{"kind":"app","namespace":"shop",
//! "app":"web"}`, rather than in the path: `app:shop/web` has a slash in it.

use super::*;

use crate::proposal::lease::{
    AgentLease, DEFAULT_LEASE_TTL_MS, LeaseAnswer, LeaseHolder, LeaseListing, LeaseOutcome,
    LeaseRefusal, LeaseRequest, LeaseTarget, LeaseTargetRequest, RenewLeaseRequest,
    TakeLeaseRequest, clean_purpose,
};

/// `GET /v1/leases`: every live lease the caller's scope reaches. Any
/// token may read them. A token scoped to some apps sees leases on those
/// apps and on their namespaces, because a namespace lease covers them.
pub(super) async fn leases_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return resp;
    }
    let Some(council) = &state.council else {
        return Json(LeaseListing::default()).into_response();
    };
    let now = crate::testkit::lease::now_unix_millis();
    let auth = auth.as_deref();
    let listing = council
        .read_desired(|desired| LeaseListing {
            leases: desired
                .agent_leases
                .live(now)
                .filter(|lease| lease_visible(auth, &lease.target))
                .map(|lease| lease.view(now))
                .collect(),
            generation: desired.agent_leases.generation,
        })
        .await;
    Json(listing).into_response()
}

/// Whether `auth`'s token scope reaches `target`.
pub(super) fn lease_visible(
    auth: Option<&crate::sesame::auth::AuthContext>,
    target: &LeaseTarget,
) -> bool {
    match target {
        LeaseTarget::App { namespace, app } => {
            crate::sesame::auth::authorize_scoped(auth, app, namespace).is_ok()
        }
        LeaseTarget::Namespace { namespace } => auth.is_none_or(|auth| {
            auth.scoped_namespaces
                .as_ref()
                .is_none_or(|namespaces| namespaces.contains(namespace))
        }),
    }
}

/// `POST /v1/leases`: take a lease, or renew it when the caller holds it.
/// A deployer or above, with `deploy` on the app (or every app in the
/// namespace) in its scope and `[permission]` spec.
pub(super) async fn lease_take_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
    Json(request): Json<TakeLeaseRequest>,
) -> Response {
    let auth = auth.as_deref();
    if let Err(response) = authorise_lease_write(
        &state,
        auth,
        &request.target,
        crate::config::PermissionAction::Deploy,
    )
    .await
    {
        return response;
    }
    let ttl_ms = match ttl_millis(request.ttl_seconds) {
        Ok(ttl) => ttl,
        Err(response) => return response,
    };
    let body = serde_json::to_string(&request).unwrap_or_default();
    let holder = lease_holder(auth);
    let take = LeaseRequest {
        target: request.target.clone(),
        holder,
        purpose: clean_purpose(&request.purpose),
        ttl_ms,
    };
    lease_write(
        &state,
        auth,
        directory.as_deref(),
        &headers,
        "/v1/leases",
        &body,
        &request.target,
        move |now| crate::council::types::RaftRequest::LeaseTake {
            request: take,
            observed_at_unix_ms: now,
        },
    )
    .await
}

/// `POST /v1/leases/renew`: extend the caller's own live lease.
pub(super) async fn lease_renew_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
    Json(request): Json<RenewLeaseRequest>,
) -> Response {
    let auth = auth.as_deref();
    if let Err(response) = authorise_lease_write(
        &state,
        auth,
        &request.target,
        crate::config::PermissionAction::Deploy,
    )
    .await
    {
        return response;
    }
    let ttl_ms = match ttl_millis(request.ttl_seconds) {
        Ok(ttl) => ttl,
        Err(response) => return response,
    };
    let body = serde_json::to_string(&request).unwrap_or_default();
    let holder = lease_holder(auth);
    let target = request.target.clone();
    lease_write(
        &state,
        auth,
        directory.as_deref(),
        &headers,
        "/v1/leases/renew",
        &body,
        &request.target,
        move |now| crate::council::types::RaftRequest::LeaseRenew {
            target,
            holder,
            ttl_ms,
            observed_at_unix_ms: now,
        },
    )
    .await
}

/// `POST /v1/leases/release`: give up the caller's own lease.
pub(super) async fn lease_release_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
    Json(request): Json<LeaseTargetRequest>,
) -> Response {
    let auth = auth.as_deref();
    if let Err(response) = authorise_lease_write(
        &state,
        auth,
        &request.target,
        crate::config::PermissionAction::Deploy,
    )
    .await
    {
        return response;
    }
    let body = serde_json::to_string(&request).unwrap_or_default();
    let holder = lease_holder(auth);
    let target = request.target.clone();
    lease_write(
        &state,
        auth,
        directory.as_deref(),
        &headers,
        "/v1/leases/release",
        &body,
        &request.target,
        move |now| crate::council::types::RaftRequest::LeaseRelease {
            target,
            holder,
            observed_at_unix_ms: now,
        },
    )
    .await
}

/// `POST /v1/leases/break`: end someone else's lease. A deployer or above
/// with `deploy` over the target; the record names who broke it.
pub(super) async fn lease_break_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
    Json(request): Json<LeaseTargetRequest>,
) -> Response {
    let auth = auth.as_deref();
    // Breaking a lease is a person's call: an agent token's effective
    // role is read-only, so this refuses it.
    if let Err(response) =
        crate::sesame::auth::authorize_user(auth, crate::sesame::types::ApiRole::Deployer)
    {
        return response;
    }
    if let Err(response) = authorise_lease_write(
        &state,
        auth,
        &request.target,
        crate::config::PermissionAction::Deploy,
    )
    .await
    {
        return response;
    }
    let body = serde_json::to_string(&request).unwrap_or_default();
    let by = lease_holder(auth);
    let target = request.target.clone();
    lease_write(
        &state,
        auth,
        directory.as_deref(),
        &headers,
        "/v1/leases/break",
        &body,
        &request.target,
        move |now| crate::council::types::RaftRequest::LeaseBreak {
            target,
            by,
            observed_at_unix_ms: now,
        },
    )
    .await
}

/// The caller as a lease holder. Before any token exists, the node's
/// bootstrap caller.
fn lease_holder(auth: Option<&crate::sesame::auth::AuthContext>) -> LeaseHolder {
    let (principal_id, name) = audit_principal(auth);
    LeaseHolder { name, principal_id }
}

/// A deployer or above (never the internal service principal), whose
/// scope and `[permission]` spec grant `action` on the target: on the app,
/// or on every app (`*`) in the namespace for a namespace lease.
///
/// An agent token granted `Deployer` may take, renew and release a lease
/// (#1682); only a person's token breaks one.
#[allow(clippy::result_large_err)]
async fn authorise_lease_write(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    target: &LeaseTarget,
    action: crate::config::PermissionAction,
) -> Result<(), Response> {
    crate::sesame::auth::authorize_proposer(auth, crate::sesame::types::ApiRole::Deployer)?;
    if let Err(refusal) = target.validate() {
        return Err(refusal_response(&refusal));
    }
    let namespace = target.namespace_name();
    let app = target.app_name().unwrap_or("*");
    crate::bun::audit::annotate(crate::bun::audit::AuditNote {
        app: target.app_name().map(str::to_string),
        namespace: Some(namespace.to_string()),
        details: std::collections::BTreeMap::new(),
    });
    crate::sesame::auth::authorize_scoped(auth, app, namespace)?;
    enforce_permission(state, auth, action, app, namespace).await
}

/// `ttl_seconds` in milliseconds, or the default; 400 outside 30..=900.
#[allow(clippy::result_large_err)]
fn ttl_millis(ttl_seconds: Option<u64>) -> Result<u64, Response> {
    use crate::proposal::lease::{MAX_LEASE_TTL_MS, MIN_LEASE_TTL_MS};
    let Some(seconds) = ttl_seconds else {
        return Ok(DEFAULT_LEASE_TTL_MS);
    };
    let millis = seconds.saturating_mul(1000);
    if (MIN_LEASE_TTL_MS..=MAX_LEASE_TTL_MS).contains(&millis) {
        return Ok(millis);
    }
    Err((
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": format!(
                "ttl_seconds must be between {} and {}",
                MIN_LEASE_TTL_MS / 1000,
                MAX_LEASE_TTL_MS / 1000
            ),
            "code": "invalid",
        })),
    )
        .into_response())
}

/// Forward to the leader, or write `build(now)` here as the leader, then
/// audit and answer the outcome.
#[allow(clippy::too_many_arguments)]
async fn lease_write(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    directory: Option<&LeaderDirectory>,
    headers: &HeaderMap,
    path: &str,
    body: &str,
    target: &LeaseTarget,
    build: impl FnOnce(u64) -> crate::council::types::RaftRequest,
) -> Response {
    let Some(council) = &state.council else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "leases live in the cluster's council; this node is standalone",
                "code": "invalid",
            })),
        )
            .into_response();
    };
    if let Some(forwarded) =
        super::upgrade::forward_upgrade_to_leader(state, council, directory, path, headers, body)
            .await
    {
        return forwarded;
    }
    let now = crate::testkit::lease::now_unix_millis();
    let outcome = match council.write(build(now)).await {
        Ok(crate::council::types::CouncilResponse::AgentLease(outcome)) => *outcome,
        Ok(other) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::json!({ "error": format!("unexpected council answer: {other:?}") }),
                ),
            )
                .into_response();
        }
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": format!("could not update the cluster: {error}"),
                    "code": "council_unavailable",
                })),
            )
                .into_response();
        }
    };
    record_lease_outcome(state, auth, target, &outcome).await;
    let (word, lease) = match outcome {
        LeaseOutcome::Taken { lease } => ("taken", lease),
        LeaseOutcome::Renewed { lease } => ("renewed", lease),
        LeaseOutcome::Released { lease } => ("released", lease),
        LeaseOutcome::Broken { lease } => ("broken", lease),
        LeaseOutcome::Refused { refusal } => return refusal_response(&refusal),
        LeaseOutcome::Expired { .. } => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "a caller's write expired leases" })),
            )
                .into_response();
        }
    };
    Json(LeaseAnswer {
        outcome: word.to_string(),
        lease: lease.view(now),
    })
    .into_response()
}

/// The HTTP answer for a refusal, with a `code` an agent can branch on:
/// `lease_held` carries the lease in the way.
fn refusal_response(refusal: &LeaseRefusal) -> Response {
    let now = crate::testkit::lease::now_unix_millis();
    let (status, code) = match refusal {
        LeaseRefusal::Held { .. } => (StatusCode::CONFLICT, "lease_held"),
        LeaseRefusal::NotFound { .. } => (StatusCode::NOT_FOUND, "not_found"),
        LeaseRefusal::Expired { .. } => (StatusCode::CONFLICT, "lease_expired"),
        LeaseRefusal::Pinned { .. } => (StatusCode::CONFLICT, "lease_pinned"),
        LeaseRefusal::HolderLimit { .. } | LeaseRefusal::ClusterLimit { .. } => {
            (StatusCode::CONFLICT, "lease_limit")
        }
        LeaseRefusal::Invalid { .. } => (StatusCode::BAD_REQUEST, "invalid"),
    };
    let mut body = serde_json::json!({ "error": refusal.to_string(), "code": code });
    if let LeaseRefusal::Held { lease } = refusal {
        body["lease"] = serde_json::to_value(lease.view(now)).unwrap_or_default();
    }
    (status, Json(body)).into_response()
}

/// The audit record for what a lease write did.
pub(super) async fn record_lease_outcome(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    target: &LeaseTarget,
    outcome: &LeaseOutcome,
) {
    use crate::bun::events::EventSeverity::{Info, Warning};
    let (action, severity, lease, message) = match outcome {
        LeaseOutcome::Taken { lease } => ("lease.taken", Info, Some(lease), "took".to_string()),
        LeaseOutcome::Renewed { lease } => {
            ("lease.renewed", Info, Some(lease), "renewed".to_string())
        }
        LeaseOutcome::Released { lease } => {
            ("lease.released", Info, Some(lease), "released".to_string())
        }
        LeaseOutcome::Broken { lease } => (
            "lease.broken",
            Warning,
            Some(lease),
            format!("broke {}'s lease on", lease.holder.name),
        ),
        LeaseOutcome::Refused { refusal } => (
            "lease.refused",
            Warning,
            match refusal {
                LeaseRefusal::Held { lease } => Some(lease.as_ref()),
                _ => None,
            },
            format!("was refused ({refusal}) a lease on"),
        ),
        LeaseOutcome::Expired { .. } => return,
    };
    let mut details = lease.map(AgentLease::audit_details).unwrap_or_default();
    details.insert("target".to_string(), target.key());
    if let LeaseOutcome::Refused { refusal } = outcome {
        details.insert("reason".to_string(), refusal.to_string());
    }
    let (_, token_name) = audit_principal(auth);
    record_caller_action(
        state,
        auth,
        CallerAction {
            kind: crate::bun::events::EventKind::Operation,
            severity,
            action,
            app: target.app_name().map(str::to_string),
            namespace: Some(target.namespace_name().to_string()),
            node: None,
            details,
            message: format!("{token_name} {message} {target}"),
        },
    )
    .await;
}

/// Why a person's direct write can't go ahead past the leases on its apps.
#[derive(Debug)]
pub(super) enum LeaseWriteRefusal {
    /// Someone else holds this lease, and the write didn't ask to break it.
    Held(Box<AgentLease>),
    /// The council couldn't be written to break a lease.
    Unavailable(String),
    /// The leases kept changing while the leader checked them.
    Churn,
}

impl LeaseWriteRefusal {
    /// The sentence a streamed apply reports.
    pub(super) fn message(&self) -> String {
        let now = crate::testkit::lease::now_unix_millis();
        match self {
            LeaseWriteRefusal::Held(lease) => crate::bun::lease_fence::held_message(lease, now),
            LeaseWriteRefusal::Unavailable(error) => {
                format!("could not break the lease: {error}")
            }
            LeaseWriteRefusal::Churn => {
                "the leases on these apps kept changing; try again".to_string()
            }
        }
    }

    /// The HTTP answer: 409 `lease_held` with the lease, or 503.
    pub(super) fn into_response(self) -> Response {
        let now = crate::testkit::lease::now_unix_millis();
        match self {
            LeaseWriteRefusal::Held(lease) => (
                StatusCode::CONFLICT,
                Json(crate::bun::lease_fence::held_body(&lease, now)),
            )
                .into_response(),
            LeaseWriteRefusal::Unavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": self.message(), "code": "council_unavailable" })),
            )
                .into_response(),
            LeaseWriteRefusal::Churn => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": self.message(), "code": "conflict" })),
            )
                .into_response(),
        }
    }
}

/// Before a person's direct write of `apps`, on the leader: the lease
/// generation to fence the write on, once no one else's lease is in the
/// way. With `break_lease`, each lease in the way is broken first and
/// audited as `lease.broken`; without it, the first one refuses the write.
pub(super) async fn clear_leases_for_write(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    auth: Option<&crate::sesame::auth::AuthContext>,
    apps: &[crate::meat::AppId],
    break_lease: bool,
) -> Result<u64, LeaseWriteRefusal> {
    let by = lease_holder(auth);
    for _ in 0..crate::bun::lease_fence::LEASE_ATTEMPTS {
        let now = crate::testkit::lease::now_unix_millis();
        let (generation, blocking) =
            crate::bun::lease_fence::blockers(council, apps, &by.principal_id, now).await;
        let Some(first) = blocking.first() else {
            return Ok(generation);
        };
        if !break_lease {
            return Err(LeaseWriteRefusal::Held(Box::new(first.clone())));
        }
        let broken = crate::bun::lease_fence::break_leases(council, &blocking, by.clone())
            .await
            .map_err(|error| LeaseWriteRefusal::Unavailable(error.to_string()))?;
        for lease in broken {
            let target = lease.target.clone();
            record_lease_outcome(state, auth, &target, &LeaseOutcome::Broken { lease }).await;
        }
    }
    Err(LeaseWriteRefusal::Churn)
}
