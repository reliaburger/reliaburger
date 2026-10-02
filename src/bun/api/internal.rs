//! Node-to-node routes: decommission, endpoint withdrawal receipts,
//! workload CSRs, producer retirement and placements.

use super::*;

/// Retire an identity only on an explicit, authenticated operator attestation.
pub(super) async fn node_decommission_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<crate::cluster::retirement::DecommissionRequest>,
) -> Response {
    use crate::council::{CouncilResponse, RaftRequest};
    let Some(auth) = auth.as_deref() else {
        return (
            StatusCode::UNAUTHORIZED,
            "an authenticated operator is required",
        )
            .into_response();
    };
    if let Err(response) =
        crate::sesame::auth::authorize_user(Some(auth), crate::sesame::types::ApiRole::Admin)
    {
        return response;
    }
    if let Err(response) = crate::sesame::auth::require_unscoped(Some(auth)) {
        return response;
    }
    if let Err(response) =
        enforce_cluster_permission(&state, Some(auth), crate::config::PermissionAction::Admin).await
    {
        return response;
    }
    if let Err(error) = request.validate() {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "decommissioning requires a cluster council",
        )
            .into_response();
    };
    if !confirmed_lease_leader(council).await {
        return forward_test_lease_request(
            &state,
            council,
            reqwest::Method::POST,
            "/v1/nodes/decommission",
            &headers,
            Some(&request),
        )
        .await;
    }
    let (is_self, membership_log_id) = {
        let metrics = council.metrics();
        let metrics = metrics.borrow();
        (
            metrics
                .membership_config
                .membership()
                .get_node(&metrics.id)
                .is_some_and(|node| node.name == request.node_id),
            *metrics.membership_config.log_id(),
        )
    };
    if is_self {
        return (
            StatusCode::CONFLICT,
            "stop or fence the target and retry through a surviving leader",
        )
            .into_response();
    }
    let write = council.write(RaftRequest::DecommissionNode {
        node_id: request.node_id,
        retired_by: auth.principal_id.clone(),
        reason: request.reason,
        retired_at_unix_ms: crate::testkit::lease::now_unix_millis(),
        membership_log_id,
    });
    match tokio::time::timeout(std::time::Duration::from_secs(10), write).await {
        Ok(Ok(CouncilResponse::NodeDecommissioned { retirement })) => {
            Json(retirement).into_response()
        }
        Ok(Ok(CouncilResponse::Refused { reason })) => {
            (StatusCode::CONFLICT, reason).into_response()
        }
        Ok(Ok(_)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected decommission response",
        )
            .into_response(),
        Ok(Err(error)) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "decommission outcome unknown; repeat the same request",
        )
            .into_response(),
    }
}

/// Existing TLS connections must observe an identity retirement too.
pub(super) async fn refuse_retired_tls_peer(
    State(state): State<ApiState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let (Some(council), Some(peer)) = (
        &state.council,
        request
            .extensions()
            .get::<crate::sesame::renewal::TlsPeerCertificate>(),
    ) {
        // An identity we can't read might belong to a retired node, so refuse it.
        let Ok(uris) = crate::sesame::cert::subject_uri_sans(&peer.0) else {
            return (
                StatusCode::FORBIDDEN,
                "peer certificate identity is unreadable",
            )
                .into_response();
        };
        let mut retired = false;
        for node in uris
            .iter()
            .filter_map(|uri| crate::sesame::ca::node_id_from_spiffe_uri(uri))
        {
            retired |= council.is_node_retired(node).await;
        }
        if retired {
            return (
                StatusCode::FORBIDDEN,
                "node identity is retired; fresh enrolment is required",
            )
                .into_response();
        }
    }
    next.run(request).await
}

/// Receipts must reach the leader directly, preserving the consumer's TLS identity.
pub(super) async fn endpoint_withdrawal_receipt_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    peer: Option<axum::Extension<crate::sesame::renewal::TlsPeerCertificate>>,
    State(state): State<ApiState>,
    Json(receipt): Json<crate::onion::withdrawal::EndpointWithdrawalReceipt>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    if let Err(error) = receipt.compatibility.require_current() {
        return (StatusCode::CONFLICT, error.to_string()).into_response();
    }
    let Some(peer) = peer else {
        return (
            StatusCode::FORBIDDEN,
            "endpoint receipts require a TLS node certificate",
        )
            .into_response();
    };
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no endpoint council available",
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
        council
            .write(crate::council::RaftRequest::AcknowledgeEndpointWithdrawal {
                node_id,
                generation: receipt.generation,
            })
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))
    })
    .await;
    match result {
        Ok(Ok(crate::council::CouncilResponse::Applied { .. })) => {
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Ok(crate::council::CouncilResponse::Refused { reason })) => {
            (StatusCode::CONFLICT, reason).into_response()
        }
        Ok(Ok(_)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "endpoint receipt is unconfirmed",
        )
            .into_response(),
        Ok(Err(error)) => error.into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "endpoint receipt outcome unknown; repeat the same receipt",
        )
            .into_response(),
    }
}

/// Producers contact the leader directly so forwarding cannot replace their TLS identity.
/// `POST /v1/cluster/workload-csr` — sign a workload CSR for a follower.
///
/// Only the leader can sign (the CA read is linearised and the serial comes
/// from Raft). The caller is identified by its node certificate, and the
/// SPIFFE identity is derived from the instance id, which must belong to an
/// app scheduled on that node.
pub(super) async fn workload_csr_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    peer: Option<axum::Extension<crate::sesame::renewal::TlsPeerCertificate>>,
    State(state): State<ApiState>,
    Json(request): Json<crate::cluster::workload_identity::WorkloadCsrRequest>,
) -> Response {
    use crate::cluster::workload_identity::{SignedWorkload, WorkloadCsrResponse, authorise};
    use base64::Engine as _;
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    if let Err(error) = request.compatibility.require_current() {
        return (StatusCode::CONFLICT, error.to_string()).into_response();
    }
    let Some(peer) = peer else {
        return (
            StatusCode::FORBIDDEN,
            "workload signing requires a TLS node certificate",
        )
            .into_response();
    };
    let Some(council) = &state.council else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no council available").into_response();
    };
    let Ok(csr_der) = base64::engine::general_purpose::STANDARD.decode(&request.csr_der) else {
        return (StatusCode::BAD_REQUEST, "workload CSR is not base64").into_response();
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let security = council
            .security_state_linearizable()
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?;
        let node_id = crate::sesame::renewal::validate_peer(&peer, &security)
            .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        let desired = council.desired_state().await;
        let (namespace, name) = authorise(
            &desired,
            &node_id,
            &request.instance_id,
            request.workload_type,
        )
        .map_err(|reason| (StatusCode::FORBIDDEN, reason))?;
        let spiffe_uri = crate::bun::agent::workload_spiffe_uri(
            &state.trust_domain,
            &namespace,
            &name,
            request.workload_type,
        );
        council
            .sign_workload_csr(
                &csr_der,
                &spiffe_uri,
                crate::sesame::identity::CertUsage::Mtls,
                &state.trust_domain,
                &node_id,
                &request.instance_id,
            )
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))
    })
    .await;
    match result {
        Ok(Ok(signed)) => Json(WorkloadCsrResponse::encode(&SignedWorkload {
            cert_der: signed.cert_der,
            workload_ca_cert_der: signed.workload_ca_cert_der,
            root_ca_cert_der: signed.root_ca_cert_der,
            jwt_token: signed.jwt_token,
        }))
        .into_response(),
        Ok(Err(error)) => error.into_response(),
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, "workload signing timed out").into_response(),
    }
}

pub(super) async fn producer_retirement_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    peer: Option<axum::Extension<crate::sesame::renewal::TlsPeerCertificate>>,
    State(state): State<ApiState>,
    Json(receipt): Json<crate::onion::producer::ProducerRetirementRequest>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    if let Err(error) = receipt.compatibility.require_current() {
        return (StatusCode::CONFLICT, error.to_string()).into_response();
    }
    let Some(peer) = peer else {
        return (
            StatusCode::FORBIDDEN,
            "producer retirement requires a TLS node certificate",
        )
            .into_response();
    };
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no endpoint council available",
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
        council
            .write(crate::council::RaftRequest::RetireEndpointExecution {
                node_id: node_id.clone(),
                execution: receipt.execution.clone(),
            })
            .await
            .map(|response| (node_id, response))
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))
    })
    .await;
    match result {
        Ok(Ok((
            node_id,
            crate::council::CouncilResponse::EndpointExecutionRetired { released: true },
        ))) => Json(crate::onion::producer::ProducerReleaseConfirmation {
            node_id,
            execution: receipt.execution,
        })
        .into_response(),
        Ok(Ok((
            _,
            crate::council::CouncilResponse::EndpointExecutionRetired { released: false },
        ))) => StatusCode::ACCEPTED.into_response(),
        Ok(Ok((_, crate::council::CouncilResponse::Refused { reason }))) => {
            (StatusCode::CONFLICT, reason).into_response()
        }
        Ok(Ok(_)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "producer retirement is unconfirmed",
        )
            .into_response(),
        Ok(Err(error)) => error.into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "producer retirement outcome unknown; repeat the same retirement",
        )
            .into_response(),
    }
}

/// `GET /v1/placements/{node_id}` — the apps (and per-node replica
/// counts) the leader has assigned to a node. Served from the Raft
/// state machine; reconcilers poll this every couple of seconds.
pub(super) async fn placements_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    peer: Option<axum::Extension<crate::sesame::renewal::TlsPeerCertificate>>,
    State(state): State<ApiState>,
    Path(node_id): Path<String>,
) -> Response {
    // Credential-free development clusters already expose placements. Adding
    // a retained consumer cannot authorise cleanup; receipt endpoints must
    // separately authenticate permission to discharge that obligation.
    let development_without_credentials = auth.is_none()
        && state.service_token.is_none()
        && state.cluster_http.scheme() == "http"
        && match &state.token_store {
            Some(tokens) => tokens.read().await.is_empty(),
            None => true,
        };
    if !development_without_credentials
        && let Err(response) = crate::sesame::auth::require_system(auth.as_deref())
    {
        return response;
    }
    if let Err(reason) = crate::cluster::retirement::validate_node_id(&node_id) {
        return (StatusCode::BAD_REQUEST, reason).into_response();
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "not running in cluster mode" })),
        )
            .into_response();
    };

    if !confirmed_lease_leader(council).await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "placements require a current leader",
        )
            .into_response();
    }
    let mut desired = council.desired_state().await;
    // Receipts must come from this same TLS identity. A plaintext consumer
    // could never send one, so registering it would only freeze discovery.
    let authenticated_consumer = peer.is_some();
    if let Some(peer) = peer {
        match crate::sesame::renewal::validate_peer(&peer, &desired.security_state) {
            Ok(identity) if identity == node_id => {}
            _ => {
                return (
                    StatusCode::FORBIDDEN,
                    "placement consumer does not match TLS identity",
                )
                    .into_response();
            }
        }
    } else if state.cluster_http.scheme() == "https" {
        return (
            StatusCode::FORBIDDEN,
            "placement consumers require a TLS node certificate",
        )
            .into_response();
    }
    if desired
        .security_state
        .crl
        .retired_nodes
        .contains_key(&node_id)
    {
        return (
            StatusCode::GONE,
            "node identity is retired; fresh enrolment is required",
        )
            .into_response();
    }
    // Record the contact before reading which consumers are registered. A
    // discharge takes the same lock, so either it sees this contact and
    // leaves the node alone, or it finishes first and the read below finds
    // the node unregistered.
    if authenticated_consumer {
        let recorded = {
            let mut contacts = council.consumer_contacts().lock().await;
            let now = std::time::Instant::now();
            contacts.observe_term(council.current_term(), now);
            contacts.record(&node_id, now)
        };
        if !recorded {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "endpoint consumer discharge in progress; poll again",
            )
                .into_response();
        }
        desired = council.desired_state().await;
    }
    // Registration precedes every first exposure. Once committed, a consumer
    // stays accountable until its view lease lapses and the leader discharges
    // it, or the operator permanently fences it.
    if authenticated_consumer && !desired.endpoint_consumers.contains(&node_id) {
        let registration = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            council.write(crate::council::RaftRequest::RegisterEndpointConsumer {
                node_id: node_id.clone(),
            }),
        )
        .await;
        match registration {
            Ok(Ok(crate::council::CouncilResponse::Applied { .. })) => {}
            Ok(Ok(crate::council::CouncilResponse::Refused { reason })) => {
                return (StatusCode::CONFLICT, reason).into_response();
            }
            _ => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "endpoint consumer registration is unconfirmed",
                )
                    .into_response();
            }
        }
        desired = council.desired_state().await;
        if !desired.endpoint_consumers.contains(&node_id) {
            return (StatusCode::GONE, "endpoint consumer identity was retired").into_response();
        }
    }
    let node = crate::meat::NodeId::new(&node_id);

    let mut apps = Vec::new();
    for (app_id, placements) in &desired.scheduling {
        let mut ordinals: Vec<u32> = placements
            .iter()
            .filter(|p| p.node_id == node)
            .map(|p| p.ordinal)
            .collect();
        if ordinals.is_empty() {
            continue;
        }
        ordinals.sort_unstable();
        let Some(spec) = desired.apps.get(app_id) else {
            continue; // spec deleted; placements lag briefly
        };
        apps.push(crate::cluster::orchestrate::NodeAssignment {
            name: app_id.name.clone(),
            namespace: app_id.namespace.clone(),
            ordinals,
            spec: spec.clone(),
        });
    }

    Json(crate::cluster::orchestrate::NodeAssignments {
        apps,
        retirements: desired
            .test_leases
            .values()
            .filter(|lease| {
                matches!(
                    lease.state,
                    crate::testkit::lease::TestLeaseState::Cleaning { .. }
                )
            })
            .flat_map(|lease| {
                lease
                    .placements
                    .iter()
                    .filter(|placement| {
                        placement.node_id == node && !desired.apps.contains_key(&placement.app_id)
                    })
                    .map(|placement| crate::cluster::orchestrate::LeaseRetirement {
                        lease_id: lease.lease_id.clone(),
                        placement: placement.clone(),
                    })
            })
            .collect(),
        // All discovery fields describe the same committed state; serving them
        // does not discharge any cleanup obligation.
        endpoint_generation: desired.endpoint_withdrawals.generation,
        endpoint_catalog: desired.endpoint_catalog.clone(),
        endpoint_withdrawals: desired
            .endpoint_withdrawals
            .pending
            .iter()
            .filter(|(_, withdrawal)| withdrawal.consumers.contains(&node_id))
            .map(|(generation, withdrawal)| {
                crate::onion::withdrawal::EndpointWithdrawalInstruction {
                    generation: *generation,
                    services: withdrawal.services.clone(),
                }
            })
            .collect(),
        ingress: crate::cluster::orchestrate::cluster_ingress(&desired),
    })
    .into_response()
}
