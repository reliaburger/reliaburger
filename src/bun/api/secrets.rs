//! Secret encryption routes: the public keys and key rotation, for the
//! cluster or one namespace (F05 I4).

use super::*;

/// `?namespace=` on the public-key route.
#[derive(Debug, serde::Deserialize)]
pub(super) struct PublicKeyQuery {
    namespace: Option<String>,
}

/// The audit details naming a rotation's scope.
fn scope_details(
    scope: &crate::sesame::types::AgeKeyScope,
) -> std::collections::BTreeMap<String, String> {
    match scope {
        crate::sesame::types::AgeKeyScope::ClusterWide => {
            std::collections::BTreeMap::from([("scope".to_string(), "cluster".to_string())])
        }
        crate::sesame::types::AgeKeyScope::Namespace(namespace) => {
            std::collections::BTreeMap::from([
                ("scope".to_string(), "namespace".to_string()),
                ("namespace".to_string(), namespace.clone()),
            ])
        }
    }
}

/// How a message names a rotation's scope.
fn scope_label(scope: &crate::sesame::types::AgeKeyScope) -> String {
    match scope {
        crate::sesame::types::AgeKeyScope::ClusterWide => "cluster".to_string(),
        crate::sesame::types::AgeKeyScope::Namespace(namespace) => {
            format!("namespace {namespace}")
        }
    }
}

/// Read the active public encryption recipient from locally applied state:
/// the cluster's, or with `?namespace=` that namespace's own.
///
/// Publishing a recipient grants no decryption authority. Scoped read-only
/// users may encrypt new values without gaining access to any private key.
/// A token scoped to other namespaces still can't ask about this one.
pub(super) async fn secret_public_key_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Query(query): Query<PublicKeyQuery>,
) -> Response {
    if let Err(response) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return response;
    }
    if let (Some(namespace), Some(auth)) = (&query.namespace, auth.as_deref())
        && auth
            .scoped_namespaces
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(namespace))
    {
        return (
            StatusCode::FORBIDDEN,
            format!("this token is not scoped to namespace {namespace}"),
        )
            .into_response();
    }
    let Some(council) = &state.council else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no council available").into_response();
    };
    let scope = match &query.namespace {
        Some(namespace) => crate::sesame::types::AgeKeyScope::Namespace(namespace.clone()),
        None => crate::sesame::types::AgeKeyScope::ClusterWide,
    };
    let security = council.security_state().await;
    let Some(keypair) = security
        .age_keypairs
        .iter()
        .filter(|key| key.scope == scope && !key.read_only)
        .max_by_key(|key| key.generation)
    else {
        return match &query.namespace {
            Some(namespace) => (
                StatusCode::NOT_FOUND,
                format!(
                    "namespace {namespace} has no secret key of its own: its values use the \
                     cluster key, unless [namespace.{namespace}] sets secret_key = true"
                ),
            )
                .into_response(),
            None => (
                StatusCode::SERVICE_UNAVAILABLE,
                "no active cluster encryption key available",
            )
                .into_response(),
        };
    };
    Json(crate::sesame::types::SecretPublicKey {
        public_key: keypair.public_key.clone(),
        generation: keypair.generation,
    })
    .into_response()
}

/// Rotate or finalise a secret encryption key via Raft: the cluster's, or
/// with `namespace` that namespace's own.
pub(super) async fn secret_rotate_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    // AUTH4: rotating the cluster's secret-encryption keys is user-admin
    // work, not something the service principal should ever do.
    if let Err(resp) =
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    // F05 I4 (decision 4): a namespace's key too stays with unscoped
    // Admins; an Admin scoped to that namespace can't rotate it.
    if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return response;
    }
    if let Err(response) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::SecretWrite,
    )
    .await
    {
        return response;
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RotateRequest {
        #[serde(default)]
        finalize: bool,
        #[serde(default)]
        namespace: Option<String>,
    }

    // A malformed body must not silently become a (non-finalise) rotation —
    // that mutates cluster key state on a typo (PKI8). An empty body keeps the
    // convenient default; anything present must parse.
    let req: RotateRequest = if body.trim().is_empty() {
        RotateRequest {
            finalize: false,
            namespace: None,
        }
    } else {
        match serde_json::from_str(&body) {
            Ok(req) => req,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": format!("invalid rotate request: {e}") })),
                )
                    .into_response();
            }
        }
    };

    let Some(ref council) = state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council available" })),
        )
            .into_response();
    };

    let scope = match &req.namespace {
        Some(namespace) => crate::sesame::types::AgeKeyScope::Namespace(namespace.clone()),
        None => crate::sesame::types::AgeKeyScope::ClusterWide,
    };
    // A namespace gets its first key by opting in, which also re-seals its
    // values; rotating can't stand in for that.
    if let Some(namespace) = &req.namespace
        && !council.security_state().await.has_namespace_key(namespace)
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "namespace {namespace} has no secret key to rotate: set secret_key = true \
                     in [namespace.{namespace}] and apply it first"
                )
            })),
        )
            .into_response();
    }
    let label = scope_label(&scope);

    if req.finalize {
        match council
            .write(crate::council::RaftRequest::FinalizeSecretRotation {
                scope: scope.clone(),
            })
            .await
        {
            // Verify-before-retire (PKI8): the state machine refuses to
            // drop the old key while any stored secret is still sealed
            // under it, and names the offenders.
            Ok(crate::council::types::CouncilResponse::Refused { reason }) => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": reason })),
            )
                .into_response(),
            Ok(_) => {
                record_caller_audit(
                    &state,
                    auth.as_deref(),
                    crate::bun::events::EventKind::Secret,
                    "secret.rotation_finalised",
                    scope_details(&scope),
                    format!("{label} secret rotation finalised; old keys retired"),
                )
                .await;
                Json(serde_json::json!({
                    "message": format!("{label} secret rotation finalised, old keys removed")
                }))
                .into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response(),
        }
    } else {
        // Generate a new age keypair
        let ikm = match council.wrapping_ikm() {
            Some(ikm) => ikm,
            None => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({ "error": "no wrapping IKM" })),
                )
                    .into_response();
            }
        };

        let security_state = council.security_state().await;
        let current_gen = security_state
            .age_keypairs
            .iter()
            .filter(|kp| kp.scope == scope)
            .map(|kp| kp.generation)
            .max()
            .unwrap_or(0);

        let new_gen = current_gen + 1;
        let (new_keypair, _identity) =
            match crate::sesame::secret::generate_age_keypair(scope.clone(), ikm, new_gen) {
                Ok(pair) => pair,
                Err(e) => {
                    return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": format!("keypair generation failed: {e}") })),
                )
                    .into_response();
                }
            };

        let new_pubkey = new_keypair.public_key.clone();

        match council
            .write(crate::council::RaftRequest::RotateSecretKey {
                scope: scope.clone(),
                new_keypair,
                resealed: Vec::new(),
            })
            .await
        {
            // One rotation at a time (PKI8): an un-finalised rotation
            // must be finalised (or re-encrypted then finalised) first.
            Ok(crate::council::types::CouncilResponse::Refused { reason }) => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": reason })),
            )
                .into_response(),
            Ok(_) => {
                let mut details = scope_details(&scope);
                details.insert("generation".to_string(), new_gen.to_string());
                record_caller_audit(
                    &state,
                    auth.as_deref(),
                    crate::bun::events::EventKind::Secret,
                    "secret.rotated",
                    details,
                    format!("{label} secret key rotated to generation {new_gen}"),
                )
                .await;
                Json(serde_json::json!({
                    "message": format!("{label} secret key rotated to generation {new_gen}"),
                    "new_public_key": new_pubkey,
                }))
                .into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response(),
        }
    }
}
