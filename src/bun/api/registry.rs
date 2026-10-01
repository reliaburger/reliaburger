//! Image routes: the image list, and the Pickle registry's council
//! proposals and queries.

use super::*;

/// Include request-body extraction in the control-operation deadline.
pub(super) async fn registry_proposal_deadline(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    match tokio::time::timeout(std::time::Duration::from_secs(10), next.run(request)).await {
        Ok(response) => response,
        Err(_) => (
            StatusCode::REQUEST_TIMEOUT,
            "registry proposal deadline exceeded",
        )
            .into_response(),
    }
}

/// A follower refuses instead of forwarding a request under its own identity.
pub(super) async fn registry_proposal_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    peer: Option<axum::Extension<crate::sesame::renewal::TlsPeerCertificate>>,
    State(state): State<ApiState>,
    Json(proposal): Json<crate::pickle::authority::RegistryProposal>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    if let Err(error) = proposal.compatibility.require_current() {
        return (StatusCode::CONFLICT, error.to_string()).into_response();
    }
    let Some(peer) = peer else {
        return (
            StatusCode::FORBIDDEN,
            "registry proposals require a TLS node certificate",
        )
            .into_response();
    };
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no registry council available",
        )
            .into_response();
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let security = council
            .security_state_linearizable()
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?;
        let node_id = crate::sesame::renewal::validate_peer(&peer, &security)
            .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        let request = proposal
            .mutation
            .request_for_node(&node_id, crate::testkit::lease::now_unix_millis())
            .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        council
            .write(request)
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))
    })
    .await;
    match result {
        Ok(Ok(response @ crate::council::CouncilResponse::Refused { .. })) => {
            (StatusCode::CONFLICT, Json(response)).into_response()
        }
        Ok(Ok(response)) => Json(response).into_response(),
        Ok(Err(error)) => error.into_response(),
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, "registry proposal timed out").into_response(),
    }
}

pub(super) async fn registry_query_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    peer: Option<axum::Extension<crate::sesame::renewal::TlsPeerCertificate>>,
    State(state): State<ApiState>,
    Json(request): Json<crate::pickle::authority::RegistryQueryRequest>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    if let Err(error) = request.compatibility.require_current() {
        return (StatusCode::CONFLICT, error.to_string()).into_response();
    }
    let Some(peer) = peer else {
        return (
            StatusCode::FORBIDDEN,
            "registry queries require a TLS node certificate",
        )
            .into_response();
    };
    let Some(council) = &state.council else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let security = match council.security_state_linearizable().await {
        Ok(security) => security,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    let node = match crate::sesame::renewal::validate_peer(&peer, &security) {
        Ok(node) => node,
        Err(error) => return (StatusCode::FORBIDDEN, error.to_string()).into_response(),
    };
    if crate::cluster::identity::raft_id_from_name(&node) != request.node_id {
        return (
            StatusCode::FORBIDDEN,
            "registry query does not belong to authenticated node",
        )
            .into_response();
    }
    let answer = request
        .query
        .answer(&council.desired_state().await, request.node_id);
    bounded_registry_query_response(answer).await
}

pub(super) async fn bounded_registry_query_response(
    answer: crate::pickle::authority::RegistryQueryResponse,
) -> Response {
    let encoded = tokio::task::spawn_blocking(move || serde_json::to_vec(&answer)).await;
    match encoded {
        Ok(Ok(bytes)) if bytes.len() <= crate::pickle::authority::MAX_REGISTRY_PROPOSAL_BYTES => (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        Ok(Ok(_)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "registry query result exceeds the control-message limit",
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `GET /v1/images` — list committed images using current cluster authority,
/// trimmed to the repositories the caller's token scope may pull.
pub(super) async fn images_handler(
    State(state): State<ApiState>,
    authority: Option<axum::Extension<crate::pickle::authority::RegistryReadAuthority>>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
) -> Response {
    use crate::pickle::authority::{RegistryQuery, RegistryQueryResponse};
    let mut images = if let Some(authority) = authority {
        match authority
            .forwarder
            .query(
                state.council.as_ref(),
                authority.node_id,
                RegistryQuery::Images,
            )
            .await
        {
            Ok(RegistryQueryResponse::Images(images)) => images,
            Ok(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "invalid registry image-list response",
                )
                    .into_response();
            }
            Err(error) => {
                return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
            }
        }
    } else if let Some(council) = &state.council {
        if let Err(error) = council.security_state_linearizable().await {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
        council.manifest_catalog().await.images()
    } else if state.static_capabilities.cluster_mode {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "registry authority is unavailable",
        )
            .into_response();
    } else if let Some(catalog) = &state.pickle_catalog {
        catalog.read().await.images()
    } else {
        Vec::new()
    };
    // The registry refuses a scoped token another namespace's repositories;
    // listing them here would hand over their names, tags and digests anyway.
    images.retain(|image| {
        crate::pickle::registry_auth::check_repository_scope(
            auth.as_deref(),
            &image.repository,
            crate::pickle::registry_auth::RepositoryAccess::Read,
        )
        .is_ok()
    });
    Json(serde_json::json!({ "images": images })).into_response()
}
