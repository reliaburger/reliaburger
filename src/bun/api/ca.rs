//! Rotating an intermediate CA (F04 R4): the council's half of
//! `relish ca rotate`.
//!
//! `prepare` makes the new key and answers with a CSR; the operator signs it
//! with the root key they hold and sends the certificate to `begin`; once
//! every node has moved, `finalize` retires the old CA. The root key never
//! reaches the cluster, and the new private key never leaves it.

use super::*;
use crate::sesame::ca_rotation::{self, PreparedRotation, RotationRole, SignedIntermediate};
use crate::sesame::types::CaRole;

/// The checks every rotation route shares: an unscoped Admin user (never the
/// internal service principal) holding cluster-wide `action`, and a council
/// to ask. Each handler names its action, so the route matrix's static guard
/// can see it.
async fn authorise(
    auth: Option<&crate::sesame::auth::AuthContext>,
    state: &ApiState,
    action: crate::config::PermissionAction,
) -> Result<Arc<crate::council::CouncilNode>, Response> {
    crate::sesame::auth::authorize_user(auth, crate::sesame::types::ApiRole::Admin)?;
    crate::sesame::auth::require_unscoped(auth)?;
    enforce_cluster_permission(state, auth, action).await?;
    state
        .council
        .clone()
        .ok_or_else(|| error_response(StatusCode::SERVICE_UNAVAILABLE, "no council available"))
}

fn error_response(status: StatusCode, error: impl std::fmt::Display) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}

/// A council write that failed. A follower can't propose, so it names the
/// leader to ask instead.
fn write_failed(error: crate::council::CouncilError) -> Response {
    match error {
        crate::council::CouncilError::ForwardToLeader { leader } => error_response(
            StatusCode::MISDIRECTED_REQUEST,
            format!(
                "this node is not the council leader; run the command against {}",
                leader
                    .as_deref()
                    .unwrap_or("the leader once one is elected")
            ),
        ),
        other => error_response(StatusCode::SERVICE_UNAVAILABLE, other),
    }
}

fn parse<T: serde::de::DeserializeOwned>(body: &str) -> Result<T, Response> {
    serde_json::from_str(body).map_err(|error| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid CA rotation request: {error}"),
        )
    })
}

fn role_details(role: CaRole) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([("role".to_string(), role.to_string())])
}

/// `POST /v1/ca/rotation/prepare`: make the role's next key and return its
/// CSR. Asking again replaces the earlier CSR.
pub(super) async fn ca_rotation_prepare_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    use base64::Engine as _;
    let council = match authorise(
        auth.as_deref(),
        &state,
        crate::config::PermissionAction::Admin,
    )
    .await
    {
        Ok(council) => council,
        Err(response) => return response,
    };
    let request: RotationRole = match parse(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let role = request.role;
    if role == CaRole::Root {
        return error_response(
            StatusCode::CONFLICT,
            ca_rotation::CaRotationError::RootRotationUnsupported,
        );
    }
    let Some(ikm) = council.wrapping_ikm().copied() else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "no wrapping key available");
    };
    let security = match council.security_state_linearizable().await {
        Ok(security) => security,
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    let (Some(active), Some(root)) = (security.active_ca(role), security.active_ca(CaRole::Root))
    else {
        return error_response(
            StatusCode::CONFLICT,
            format!("the cluster has no active {role} CA and root to rotate from"),
        );
    };
    let generation = active.generation + 1;
    let root_fingerprint =
        crate::sesame::identity_store::root_ca_fingerprint(&root.certificate_der);

    let made =
        tokio::task::spawn_blocking(move || crate::sesame::ca::create_intermediate_csr(role, &ikm))
            .await;
    let (csr_der, private_key_wrapped) = match made {
        Ok(Ok(made)) => made,
        Ok(Err(error)) => return error_response(StatusCode::BAD_REQUEST, error),
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let csr_b64 = base64::engine::general_purpose::STANDARD.encode(&csr_der);
    let serial = match council
        .write(crate::council::RaftRequest::CaRotationPrepare {
            role,
            generation,
            csr_der,
            private_key_wrapped,
        })
        .await
    {
        Ok(crate::council::CouncilResponse::SerialAllocated { serial }) => serial,
        Ok(crate::council::CouncilResponse::Refused { reason }) => {
            return error_response(StatusCode::CONFLICT, reason);
        }
        Ok(other) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("unexpected council response: {other:?}"),
            );
        }
        Err(error) => return write_failed(error),
    };
    record_caller_audit(
        &state,
        auth.as_deref(),
        crate::bun::events::EventKind::Identity,
        "ca.rotation_prepared",
        role_details(role),
        format!("{role} CA rotation prepared: generation {generation} awaits the root's signature"),
    )
    .await;
    Json(PreparedRotation {
        role,
        generation,
        serial,
        csr_b64,
        root_fingerprint,
    })
    .into_response()
}

/// `POST /v1/ca/rotation/begin`: check the operator's certificate against
/// the pending CSR and begin the rotation. Both CAs are trusted from here
/// on, and new leaves come from the new one.
pub(super) async fn ca_rotation_begin_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    use base64::Engine as _;
    let council = match authorise(
        auth.as_deref(),
        &state,
        crate::config::PermissionAction::Admin,
    )
    .await
    {
        Ok(council) => council,
        Err(response) => return response,
    };
    let request: SignedIntermediate = match parse(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let role = request.role;
    let certificate_der =
        match base64::engine::general_purpose::STANDARD.decode(&request.certificate_b64) {
            Ok(der) => der,
            Err(error) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    format!("invalid certificate: {error}"),
                );
            }
        };
    let security = match council.security_state_linearizable().await {
        Ok(security) => security,
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    let new_ca = match ca_rotation::intermediate_from_signed(&security, role, &certificate_der) {
        Ok(new_ca) => new_ca,
        Err(error) => return error_response(StatusCode::CONFLICT, error),
    };
    let generation = new_ca.generation;
    match council
        .write(crate::council::RaftRequest::CaRotationBegin {
            role,
            ca: Box::new(new_ca),
        })
        .await
    {
        Ok(crate::council::CouncilResponse::Refused { reason }) => {
            error_response(StatusCode::CONFLICT, reason)
        }
        Ok(_) => {
            record_caller_audit(
                &state,
                auth.as_deref(),
                crate::bun::events::EventKind::Identity,
                "ca.rotation_begun",
                role_details(role),
                format!("{role} CA rotation begun: generation {generation} signs new leaves"),
            )
            .await;
            Json(serde_json::json!({
                "message": format!(
                    "{role} CA rotation begun: generation {generation} signs new leaves, and \
                     both CAs are trusted until you finalise"
                ),
                "generation": generation,
            }))
            .into_response()
        }
        Err(error) => write_failed(error),
    }
}

/// `POST /v1/ca/rotation/finalize`: retire the old CA, refused while
/// anything could still depend on it.
pub(super) async fn ca_rotation_finalize_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    let council = match authorise(
        auth.as_deref(),
        &state,
        crate::config::PermissionAction::Admin,
    )
    .await
    {
        Ok(council) => council,
        Err(response) => return response,
    };
    let request: RotationRole = match parse(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let role = request.role;
    let now_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let now_unix_ms = u64::try_from(now_unix_ms).unwrap_or(u64::MAX);
    match council
        .write(crate::council::RaftRequest::CaRotationFinalize { role, now_unix_ms })
        .await
    {
        Ok(crate::council::CouncilResponse::Refused { reason }) => {
            error_response(StatusCode::CONFLICT, reason)
        }
        Ok(_) => {
            record_caller_audit(
                &state,
                auth.as_deref(),
                crate::bun::events::EventKind::Identity,
                "ca.rotation_finalised",
                role_details(role),
                format!("{role} CA rotation finalised; the old CA is retired"),
            )
            .await;
            Json(serde_json::json!({
                "message": format!("{role} CA rotation finalised; the old CA is retired")
            }))
            .into_response()
        }
        Err(error) => write_failed(error),
    }
}
