//! Secret encryption routes: the cluster's public key and key rotation.

use super::*;

/// Read the active public encryption recipient from locally applied state.
///
/// Publishing a recipient grants no decryption authority. Scoped read-only
/// users may encrypt new values without gaining access to any private key.
pub(super) async fn secret_public_key_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(response) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return response;
    }
    let Some(council) = &state.council else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no council available").into_response();
    };
    let security = council.security_state().await;
    let Some(keypair) = security
        .age_keypairs
        .iter()
        .filter(|key| key.scope == crate::sesame::types::AgeKeyScope::ClusterWide && !key.read_only)
        .max_by_key(|key| key.generation)
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no active cluster encryption key available",
        )
            .into_response();
    };
    Json(crate::sesame::types::SecretPublicKey {
        public_key: keypair.public_key.clone(),
        generation: keypair.generation,
    })
    .into_response()
}

/// Rotate or finalise secret encryption key via Raft.
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
    struct RotateRequest {
        #[serde(default)]
        finalize: bool,
    }

    // A malformed body must not silently become a (non-finalise) rotation —
    // that mutates cluster key state on a typo (PKI8). An empty body keeps the
    // convenient default; anything present must parse.
    let req: RotateRequest = if body.trim().is_empty() {
        RotateRequest { finalize: false }
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

    let scope = crate::sesame::types::AgeKeyScope::ClusterWide;

    if req.finalize {
        match council
            .write(crate::council::RaftRequest::FinalizeSecretRotation { scope })
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
                    std::collections::BTreeMap::from([(
                        "scope".to_string(),
                        "cluster".to_string(),
                    )]),
                    "secret rotation finalised; old keys retired".to_string(),
                )
                .await;
                Json(
                    serde_json::json!({ "message": "secret rotation finalised, old keys removed" }),
                )
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
            .write(crate::council::RaftRequest::RotateSecretKey { scope, new_keypair })
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
                record_caller_audit(
                    &state,
                    auth.as_deref(),
                    crate::bun::events::EventKind::Secret,
                    "secret.rotated",
                    std::collections::BTreeMap::from([
                        ("scope".to_string(), "cluster".to_string()),
                        ("generation".to_string(), new_gen.to_string()),
                    ]),
                    format!("secret key rotated to generation {new_gen}"),
                )
                .await;
                Json(serde_json::json!({
                    "message": format!("secret key rotated to generation {new_gen}"),
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
