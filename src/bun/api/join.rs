//! Node join and certificate routes: the cluster CA, node certificate
//! renewal and the join handshake.

use super::*;

/// Issue a certificate bundle to a joining node (issuer side).
///
/// Public route: the join token is the credential. The joiner sends a CSR and
/// keeps its private key (PKI4); we sign the CSR and return the leaf plus CA
/// chain the joiner persists as its identity.
/// `GET /v1/cluster/ca` — the cluster's public CA certificates.
///
/// A joiner fetches these *before* sending its one-time join token so it can
/// verify the cluster's identity against a pinned `--ca-fingerprint` and then
/// transmit the token only over a connection proven to chain to this CA. CA
/// certificates are public material, so the endpoint needs no authentication.
pub(super) async fn cluster_ca_handler(State(state): State<ApiState>) -> Response {
    use base64::Engine as _;
    let Some(ref council) = state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council available" })),
        )
            .into_response();
    };
    let security = council.security_state().await;
    let (Some(node_ca), Some(root_ca)) = (
        security.active_ca(crate::sesame::types::CaRole::Node),
        security.active_ca(crate::sesame::types::CaRole::Root),
    ) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "cluster CA not initialised" })),
        )
            .into_response();
    };
    let encoder = base64::engine::general_purpose::STANDARD;
    // During a Node CA rotation a member may still present a leaf from the
    // retiring CA, so the joiner pins every trusted one (F04 R2).
    let trusted_node_cas: Vec<String> = security
        .trusted_cas(crate::sesame::types::CaRole::Node)
        .iter()
        .map(|ca| encoder.encode(&ca.certificate_der))
        .collect();
    Json(serde_json::json!({
        "compatibility": crate::compatibility::CURRENT,
        "node_ca_b64": encoder.encode(&node_ca.certificate_der),
        "root_ca_b64": encoder.encode(&root_ca.certificate_der),
        "trusted_node_cas_b64": trusted_node_cas,
    }))
    .into_response()
}

/// Renew only the node authenticated on this connection. A follower refuses;
/// forwarding would substitute the follower's TLS identity for the caller's.
pub(super) async fn node_renewal_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    peer: Option<axum::Extension<crate::sesame::renewal::TlsPeerCertificate>>,
    lifetime: Option<axum::Extension<crate::sesame::renewal::NodeLeafLifetime>>,
    State(state): State<ApiState>,
    Json(request): Json<crate::sesame::renewal::RenewalRequest>,
) -> Response {
    use crate::sesame::renewal::{RenewalError, issue_renewal};
    let lifetime = lifetime.map_or(crate::sesame::ca::NODE_LEAF_LIFETIME, |lifetime| {
        lifetime.0.0
    });
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    let Some(peer) = peer else {
        return (
            StatusCode::FORBIDDEN,
            "node renewal requires a TLS client certificate",
        )
            .into_response();
    };
    let Some(council) = &state.council else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no council available").into_response();
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        issue_renewal(council, &peer, &request, lifetime),
    )
    .await
    {
        Ok(Ok(bundle)) => Json(bundle).into_response(),
        Ok(Err(error)) => {
            let status = match &error {
                RenewalError::Identity(_) => StatusCode::FORBIDDEN,
                RenewalError::Request(_) => StatusCode::BAD_REQUEST,
                RenewalError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            };
            (
                status,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response()
        }
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, "node renewal timed out").into_response(),
    }
}

pub(super) async fn join_handler(
    State(state): State<ApiState>,
    Json(body): Json<crate::sesame::join::JoinRequest>,
) -> Response {
    use base64::Engine as _;
    if let Err(error) = body.compatibility.require_current() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response();
    }
    let csr_der = match base64::engine::general_purpose::STANDARD.decode(&body.csr_b64) {
        Ok(der) => der,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid CSR: {e}") })),
            )
                .into_response();
        }
    };
    match ask_agent(&state.cmd_tx, |response| AgentCommand::JoinIssue {
        token: body.token,
        node_id: body.node_id,
        csr_der,
        response,
    })
    .await
    {
        Ok(Ok(bundle)) => Json(bundle).into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(response) => response,
    }
}
