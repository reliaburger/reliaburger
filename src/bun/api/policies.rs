//! The policy routes (#1117): list the policies in force, and delete one.
//!
//! Policies are written by `relish apply` and GitOps like any declarative
//! kind. Deleting one has its own route because it must always work: a
//! policy never judges a policy, so an admin can remove one that refuses
//! everything.

use super::*;

use crate::policy::PolicyListing;

/// `GET /v1/policies` — the cluster's policies. Any token may read them.
pub(super) async fn policies_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return resp;
    }
    let Some(council) = &state.council else {
        return Json(PolicyListing::default()).into_response();
    };
    let listing = council
        .read_desired(|desired| PolicyListing {
            policies: desired.policies.clone(),
            generation: desired.policy_generation,
            gitops_owned: desired.gitops_owned.policies.clone(),
        })
        .await;
    Json(listing).into_response()
}

/// `POST /v1/policies/{name}/delete` — remove a policy. Admin only, and
/// audited. A follower passes the request, with the caller's credential,
/// to the leader.
pub(super) async fn policy_delete_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "policies live in the cluster's council; this node is standalone"
            })),
        )
            .into_response();
    };
    if let Some(forwarded) = super::upgrade::forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        &format!("/v1/policies/{name}/delete"),
        &headers,
        "",
    )
    .await
    {
        return forwarded;
    }
    match council
        .write(crate::council::types::RaftRequest::PolicyDelete { name: name.clone() })
        .await
    {
        Ok(crate::council::types::CouncilResponse::Refused { reason }) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": reason })),
        )
            .into_response(),
        Ok(_) => Json(serde_json::json!({ "status": "deleted", "policy": name })).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": format!("could not update the cluster: {error}") })),
        )
            .into_response(),
    }
}

/// `GET /v1/policies/audit` — every committed app and job the policies in
/// force would now refuse, warn about or record (#1119). It judges and
/// changes nothing. Any token may read it, as it may read the policies.
pub(super) async fn policy_audit_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return resp;
    }
    let Some(council) = &state.council else {
        return Json(crate::policy::audit::PolicyAudit::default()).into_response();
    };
    // Judging every workload is CPU work, so it runs off the async
    // threads, on a copy of desired state rather than under its lock.
    let desired = council.desired_state().await;
    match tokio::task::spawn_blocking(move || crate::policy::audit::audit(&desired)).await {
        Ok(audit) => Json(audit).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("the audit failed: {error}") })),
        )
            .into_response(),
    }
}
