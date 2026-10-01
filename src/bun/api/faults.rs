//! Chaos routes: fault injection, listing and clearing, and the node-fault
//! reservations that keep a node kill within the cluster's safety bounds.

use super::*;

/// Show the locally replicated node-experiment reservation, if any.
pub(super) async fn chaos_status_handler(State(state): State<ApiState>) -> Response {
    let reservation = match &state.council {
        Some(council) => council.desired_state().await.node_fault_reservations.active,
        None => None,
    };
    Json(serde_json::json!({
        "node_fault_reservation": reservation.map(|grant| serde_json::json!({
            "sequence": grant.sequence,
            "target_node": grant.request.target_node,
            "fault_type": grant.request.fault_type,
            "cleanup_after_unix_ms": grant.cleanup_after_unix_ms,
        })),
    }))
    .into_response()
}

/// Inject a fault (Smoker).
pub(super) async fn fault_inject_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(mut request): Json<crate::smoker::types::FaultRequest>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_user(
        auth.as_deref(),
        crate::sesame::types::ApiRole::Deployer,
    ) {
        return resp;
    }
    if request.fault_type.is_node_targeted() {
        let Some(auth) = auth.as_deref() else {
            return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        };
        let operation = if matches!(
            request.fault_type,
            crate::smoker::types::FaultType::NodePressure { .. }
        ) {
            crate::testkit::safety::OperationPermission::SaturateCapacity
        } else {
            crate::testkit::safety::OperationPermission::AlterNodeState
        };
        if let Err(response) = state.static_capabilities.test_policy.authorise(
            operation,
            &crate::testkit::safety::OperationAuthorisation {
                principal: &auth.principal_id,
                role: auth.role,
                acknowledged: request.acknowledged,
            },
        ) {
            return (StatusCode::FORBIDDEN, response.to_string()).into_response();
        }
        let Some(target_node) = request
            .target_node
            .as_deref()
            .filter(|target| !target.is_empty())
        else {
            return (
                StatusCode::BAD_REQUEST,
                "node-targeted faults require target_node",
            )
                .into_response();
        };
        if let Err(response) = check_node_fault_cluster_safety(&state, &request).await {
            return response;
        }
        // A node with no cluster identity can't decide whether it *is* the
        // target, so applying locally would pressure/kill the wrong (unnamed)
        // node. Refuse rather than mis-route.
        let Some(self_name) = state.node_name.as_deref() else {
            return (
                StatusCode::BAD_REQUEST,
                "this node has no cluster identity; cannot route node-targeted faults",
            )
                .into_response();
        };
        if self_name != target_node {
            return forward_node_fault(&state, target_node, &headers, &request).await;
        }
    } else {
        // Workload fault: normalise the namespace (apps default to `default`)
        // and enforce the caller's token scope against it, so a Deployer scoped
        // to one namespace cannot inject a fault into another tenant's
        // same-named service (AUTH1 for faults). The normalised namespace is
        // written back so the agent targets only the intended tenant.
        let namespace = request
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string());
        request.namespace = Some(namespace.clone());
        if let Err(response) = crate::sesame::auth::authorize_scoped(
            auth.as_deref(),
            &request.target_service,
            &namespace,
        ) {
            return response;
        }
        let (principal, role) = auth
            .as_deref()
            .map(|auth| (auth.principal_id.as_str(), auth.role))
            .unwrap_or(("local-bootstrap", crate::sesame::types::ApiRole::Admin));
        if let Err(response) = state.static_capabilities.test_policy.authorise(
            crate::testkit::safety::OperationPermission::InjectWorkloadFaults,
            &crate::testkit::safety::OperationAuthorisation {
                principal,
                role,
                acknowledged: request.acknowledged,
            },
        ) {
            return (StatusCode::FORBIDDEN, response.to_string()).into_response();
        }
        // Workload faults act on processes, so they have to reach the node
        // that runs them. A cluster member routes every one, including those
        // it keeps for itself, so the replica rail always sees the whole
        // service.
        if let Some(self_name) = state.node_name.clone()
            && state.membership.is_some()
        {
            return route_workload_fault(&state, auth.as_deref(), &headers, request, &self_name)
                .await;
        }
    }

    // The caller controls the JSON body, so it cannot be the audit identity.
    // Token names are already authenticated by the middleware.
    request.injected_by = auth
        .as_deref()
        .map(|auth| auth.token_name.clone())
        .unwrap_or_else(|| "local-bootstrap".to_string());
    let reservation = if request.fault_type.is_node_targeted() {
        match prepare_and_reserve_node_fault(&state, request.clone()).await {
            Ok(grant) => {
                request = grant.request.clone();
                Some(grant)
            }
            Err(response) => return *response,
        }
    } else {
        None
    };
    match apply_fault_locally(&state, auth.as_deref(), request, reservation, None).await {
        Ok(summary) => Json(summary).into_response(),
        Err(response) => response,
    }
}

/// Apply a fault on this node and record its audit event.
///
/// `replica_evidence` carries the cluster-wide replica counts a routed
/// workload fault was judged against; `None` keeps the agent's local view.
// `Response` is large but it IS the HTTP reply to send on failure;
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
pub(super) async fn apply_fault_locally(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    mut request: crate::smoker::types::FaultRequest,
    reservation: Option<crate::smoker::reservation::NodeFaultReservation>,
    replica_evidence: Option<crate::smoker::types::ReplicaEvidence>,
) -> Result<crate::smoker::types::FaultSummary, Response> {
    request.injected_by = auth
        .map(|auth| auth.token_name.clone())
        .unwrap_or_else(|| "local-bootstrap".to_string());
    let audit_principal = auth
        .map(|auth| auth.principal_id.clone())
        .unwrap_or_else(|| "local-bootstrap".to_string());
    let audit_target_node = request.target_node.clone();
    let audit_target_service = request.target_service.clone();
    let audit_target_instance = request.target_instance.clone();
    let audit_fault_type = serde_json::to_value(&request.fault_type)
        .ok()
        .and_then(|value| value.get("type")?.as_str().map(str::to_string))
        .unwrap_or_else(|| request.fault_type.to_string());
    let audit_duration_seconds = request.duration.as_secs();
    let audit_reason = request.reason.clone();
    match ask_agent(&state.cmd_tx, |response| AgentCommand::InjectFault {
        reservation: reservation.map(Box::new),
        request,
        replica_evidence,
        response,
    })
    .await
    {
        Ok(Ok(summary)) => {
            if let Some(events) = &state.events {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let mut details = std::collections::BTreeMap::from([
                    ("fault_id".to_string(), summary.id.to_string()),
                    ("fault_type".to_string(), audit_fault_type.clone()),
                    (
                        "duration_seconds".to_string(),
                        audit_duration_seconds.to_string(),
                    ),
                ]);
                if let Some(instance) = audit_target_instance {
                    details.insert("target_instance".to_string(), instance);
                }
                if let Some(reason) = audit_reason {
                    details.insert("reason".to_string(), reason);
                }
                events
                    .write()
                    .await
                    .record_audit(crate::bun::events::AuditEvent {
                        timestamp,
                        kind: crate::bun::events::EventKind::Fault,
                        severity: crate::bun::events::EventSeverity::Warning,
                        action: "fault.injected".to_string(),
                        principal: audit_principal.clone(),
                        app: (!audit_target_service.is_empty()).then_some(audit_target_service),
                        namespace: None,
                        node: audit_target_node,
                        details,
                        message: format!(
                            "fault {} ({}) injected for {}s by principal {}",
                            summary.id, summary.fault_type, audit_duration_seconds, audit_principal
                        ),
                    });
            }
            Ok(summary)
        }
        Ok(Err(e)) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response()),
        Err(response) => Err(response),
    }
}

pub(super) struct FaultAudit<'a> {
    pub(super) action: &'a str,
    pub(super) principal: &'a str,
    pub(super) severity: crate::bun::events::EventSeverity,
    pub(super) app: Option<String>,
    pub(super) node: Option<String>,
    pub(super) details: std::collections::BTreeMap<String, String>,
    pub(super) message: String,
}

pub(super) async fn record_fault_audit(state: &ApiState, audit: FaultAudit<'_>) {
    let Some(events) = &state.events else {
        return;
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    events
        .write()
        .await
        .record_audit(crate::bun::events::AuditEvent {
            timestamp,
            kind: crate::bun::events::EventKind::Fault,
            severity: audit.severity,
            action: audit.action.to_string(),
            principal: audit.principal.to_string(),
            app: audit.app,
            namespace: None,
            node: audit.node,
            details: audit.details,
            message: audit.message,
        });
}

/// Re-evaluate node safety from API-owned live cluster state before routing.
///
/// Fault registries are node-local. A killed voter is therefore counted from
/// the replicated voter set minus live SWIM members, so a request reaching a
/// different node cannot silently exceed quorum. The target agent repeats its
/// local checks immediately before applying the effect.
// `Response` is large but it IS the HTTP reply to send on failure —
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
pub(super) async fn check_node_fault_cluster_safety(
    state: &ApiState,
    request: &crate::smoker::types::FaultRequest,
) -> Result<(), Response> {
    let Some(council) = &state.council else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault safety requires live council evidence",
        )
            .into_response());
    };
    let metrics = council.metrics().borrow().clone();
    let raft_membership = metrics.membership_config.membership();
    let council_voters: std::collections::BTreeSet<_> = raft_membership.voter_ids().collect();
    if council_voters.is_empty() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault safety requires a known council membership",
        )
            .into_response());
    }
    let Some(membership) = &state.membership else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault safety requires live membership evidence",
        )
            .into_response());
    };
    let members = membership.read().await;
    let alive_voters: std::collections::BTreeSet<_> = members
        .iter()
        .map(|member| crate::cluster::identity::raft_id_from_name(&member.node_id.0))
        .collect();
    let unavailable_council_nodes = council_voters.difference(&alive_voters).count() as u32;
    let Some(leader) = metrics.current_leader else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault safety requires a known council leader",
        )
            .into_response());
    };
    let Some(leader_node_id) = members
        .iter()
        .find(|member| crate::cluster::identity::raft_id_from_name(&member.node_id.0) == leader)
        .map(|member| member.node_id.0.clone())
    else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault safety cannot map the council leader to live membership",
        )
            .into_response());
    };
    let context = crate::smoker::types::SafetyContext {
        council_size: council_voters.len() as u32,
        council_nodes_with_active_faults: unavailable_council_nodes,
        leader_node_id,
        total_nodes: members.len().max(council_voters.len()) as u32,
        nodes_with_active_faults: unavailable_council_nodes,
        target_service_replicas: 0,
        target_service_faulted_replicas: 0,
    };
    let decision = crate::smoker::safety::evaluate_safety(request, &context);
    if decision.approved {
        Ok(())
    } else {
        let reason = decision
            .violation
            .map(|violation| violation.to_string())
            .unwrap_or_else(|| "node fault safety check failed".to_string());
        Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": reason })),
        )
            .into_response())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct NodeFaultPreparation {
    pub(super) boot_id: String,
    pub(super) request: crate::smoker::types::FaultRequest,
}

/// The public endpoint remains an operator action; only a trusted target API
/// may obtain the internal grant after it has checked its own server policy.
pub(super) async fn node_fault_reserve_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Json(prepared): Json<NodeFaultPreparation>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    match reserve_node_fault_on_leader(&state, prepared).await {
        Ok(grant) => Json(grant).into_response(),
        Err(response) => *response,
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct NodeFaultFenceRequest {
    pub(super) reservation: crate::smoker::reservation::NodeFaultReservation,
    pub(super) only_if_finished: bool,
}

pub(super) async fn node_fault_fence_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Json(request): Json<NodeFaultFenceRequest>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    if request.reservation.request.target_node.as_deref() != state.node_name.as_deref() {
        return (
            StatusCode::BAD_REQUEST,
            "node fault fence targets another node",
        )
            .into_response();
    }
    match fence_node_fault_locally(&state, request.reservation, request.only_if_finished).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    }
}

pub(super) async fn prepare_and_reserve_node_fault(
    state: &ApiState,
    request: crate::smoker::types::FaultRequest,
) -> Result<crate::smoker::reservation::NodeFaultReservation, Box<Response>> {
    let operation = async {
        let (response, receiver) = oneshot::channel();
        state
            .cmd_tx
            .send(AgentCommand::PrepareNodeFault { request, response })
            .await
            .map_err(|_| "agent unavailable".to_string())?;
        receiver
            .await
            .map_err(|_| "agent dropped preparation response".to_string())?
            .map_err(|error| error.to_string())
    };
    let (boot_id, request) =
        match tokio::time::timeout(std::time::Duration::from_secs(5), operation).await {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => return Err((StatusCode::BAD_REQUEST, error).into_response().into()),
            Err(_) => {
                return Err((
                    StatusCode::GATEWAY_TIMEOUT,
                    "node fault preparation timed out",
                )
                    .into_response()
                    .into());
            }
        };
    let prepared = NodeFaultPreparation { boot_id, request };
    let Some(council) = &state.council else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault safety requires live council evidence",
        )
            .into_response()
            .into());
    };
    if council.is_leader().await {
        return reserve_node_fault_on_leader(state, prepared).await;
    }
    let Some(leader) = leader_api_url(state, council).await else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault safety requires a known council leader",
        )
            .into_response()
            .into());
    };
    let bytes = post_node_fault_internal(state, format!("{leader}/v1/chaos/reserve"), &prepared)
        .await
        .map_err(|error| Box::new((StatusCode::SERVICE_UNAVAILABLE, error).into_response()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| Box::new((StatusCode::BAD_GATEWAY, error.to_string()).into_response()))
}

pub(super) async fn reserve_node_fault_on_leader(
    state: &ApiState,
    prepared: NodeFaultPreparation,
) -> Result<crate::smoker::reservation::NodeFaultReservation, Box<Response>> {
    check_node_fault_cluster_safety(state, &prepared.request).await?;
    let council = state
        .council
        .as_ref()
        .ok_or_else(|| Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()))?;
    if !council.is_leader().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault leader changed; retry",
        )
            .into_response()
            .into());
    }
    let metrics = council.metrics().borrow().clone();
    let voters: std::collections::BTreeSet<_> =
        metrics.membership_config.membership().voter_ids().collect();
    let membership = state
        .membership
        .as_ref()
        .ok_or_else(|| Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()))?;
    let members = membership.read().await;
    if !members
        .iter()
        .any(|member| Some(member.node_id.0.as_str()) == prepared.request.target_node.as_deref())
    {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault target is not in live membership",
        )
            .into_response()
            .into());
    }
    let alive: std::collections::BTreeSet<_> = members
        .iter()
        .map(|member| crate::cluster::identity::raft_id_from_name(&member.node_id.0))
        .collect();
    drop(members);
    let ledger = council.desired_state().await.node_fault_reservations;
    let Some(sequence) = ledger.last_sequence.checked_add(1) else {
        return Err((StatusCode::CONFLICT, "node fault sequence exhausted")
            .into_response()
            .into());
    };
    let reservation = crate::smoker::reservation::NodeFaultReservation {
        sequence,
        boot_id: prepared.boot_id,
        cleanup_after_unix_ms: crate::testkit::lease::now_unix_millis()
            .saturating_add(prepared.request.duration.as_millis().min(u64::MAX as u128) as u64),
        request: prepared.request,
    };
    let write = council.write(crate::council::RaftRequest::ReserveNodeFault {
        reservation: Box::new(reservation.clone()),
        membership_log_id: *metrics.membership_config.log_id(),
        unavailable_voters: voters.difference(&alive).copied().collect(),
    });
    match tokio::time::timeout(std::time::Duration::from_secs(5), write).await {
        Ok(Ok(crate::council::CouncilResponse::Refused { reason })) => {
            Err((StatusCode::CONFLICT, reason).into_response().into())
        }
        Ok(Ok(_)) => Ok(reservation),
        Ok(Err(error)) => Err((StatusCode::SERVICE_UNAVAILABLE, error.to_string())
            .into_response()
            .into()),
        Err(_) => Err((
            StatusCode::GATEWAY_TIMEOUT,
            "node fault reservation outcome unknown; capacity retained until fenced",
        )
            .into_response()
            .into()),
    }
}

pub(super) async fn post_node_fault_internal<T: Serialize>(
    state: &ApiState,
    url: String,
    body: &T,
) -> Result<Vec<u8>, String> {
    let token = state
        .service_token
        .as_ref()
        .ok_or("node fault coordination requires a service identity")?;
    let operation = async {
        let mut response = state
            .cluster_http
            .client()
            .post(url)
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
            if bytes.len().saturating_add(chunk.len()) > MAX_FAULT_FORWARD_RESPONSE_BYTES {
                return Err("node fault coordination response exceeds 64 KiB".to_string());
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(format!(
                "node fault coordination refused ({status}): {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        Ok(bytes)
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), operation)
        .await
        .map_err(|_| "node fault coordination timed out; ownership remains reserved".to_string())?
}

pub(super) async fn fence_node_fault_locally(
    state: &ApiState,
    reservation: crate::smoker::reservation::NodeFaultReservation,
    only_if_finished: bool,
) -> Result<(), String> {
    let operation = async {
        let (response, receiver) = oneshot::channel();
        state
            .cmd_tx
            .send(AgentCommand::FenceNodeFault {
                only_if_finished,
                reservation,
                response,
            })
            .await
            .map_err(|_| "agent unavailable".to_string())?;
        receiver
            .await
            .map_err(|_| "agent dropped fence response".to_string())?
            .map_err(|error| error.to_string())
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), operation)
        .await
        .map_err(|_| "node fault fence outcome unknown".to_string())?
}

pub(super) fn spawn_node_fault_reaper(state: ApiState) {
    let Some(council) = state.council.clone() else {
        return;
    };
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _ = state.cmd_tx.closed() => return, _ = interval.tick() => {} }
            let cleanup = async {
                if !council.is_leader().await {
                    return;
                }
                let Some(grant) = council.desired_state().await.node_fault_reservations.active
                else {
                    return;
                };
                let only_if_finished =
                    grant.cleanup_after_unix_ms > crate::testkit::lease::now_unix_millis();
                let result = if grant.request.target_node.as_deref() == state.node_name.as_deref() {
                    fence_node_fault_locally(&state, grant.clone(), only_if_finished).await
                } else if let Some(target) = grant.request.target_node.as_deref() {
                    match target_node_api_url(&state, target, "/v1/chaos/fence").await {
                        Ok(url) => post_node_fault_internal(
                            &state,
                            url,
                            &NodeFaultFenceRequest {
                                reservation: grant.clone(),
                                only_if_finished,
                            },
                        )
                        .await
                        .map(|_| ()),
                        Err(_) => Err("node fault target is unavailable for fencing".to_string()),
                    }
                } else {
                    Err("node fault reservation has no target".to_string())
                };
                if result.is_ok() {
                    // A new leader either inherits this slot or sees the release.
                    // No deadline or failed acknowledgement can clear ownership.
                    let _ = council
                        .write(crate::council::RaftRequest::ReleaseNodeFault {
                            sequence: grant.sequence,
                        })
                        .await;
                }
            };
            tokio::select! {
                _ = state.cmd_tx.closed() => return,
                _ = tokio::time::timeout(std::time::Duration::from_secs(10), cleanup) => {}
            }
        }
    });
}

pub(super) const MAX_FAULT_FORWARD_RESPONSE_BYTES: usize = 64 * 1024;

/// Send a node-level operation to the named node while preserving the caller's
/// credential. The target repeats role, policy and acknowledgement checks.
pub(super) async fn forward_node_fault(
    state: &ApiState,
    target_node: &str,
    headers: &HeaderMap,
    request: &crate::smoker::types::FaultRequest,
) -> Response {
    let url = match target_node_api_url(state, target_node, "/v1/fault").await {
        Ok(url) => url,
        Err(response) => return response,
    };
    let forwarded = state.cluster_http.client().post(url).json(request);
    send_node_request(
        target_node,
        copy_forwarded_auth(forwarded, headers),
        "fault",
    )
    .await
}

/// How long a peer may take to report its instances or faults while a
/// workload fault is being routed. It stays well under the 5-second deadline
/// a forwarding node gives the owner, which gathers the same evidence again.
pub(super) const FAULT_EVIDENCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Route a workload fault to the nodes that run its targets.
///
/// The node that receives the request plans one request per owner from live
/// cluster status, checks the replica rail against cluster-wide counts, then
/// applies its own share and forwards the rest under the caller's credential.
/// An owner receiving a forwarded share (its `target_node` names the owner)
/// repeats the same steps, so its own server policy and its own view of the
/// replica rail decide before anything happens there.
pub(super) async fn route_workload_fault(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    headers: &HeaderMap,
    request: crate::smoker::types::FaultRequest,
    self_name: &str,
) -> Response {
    use crate::smoker::routing::{WorkloadInstance, plan_workload_fault, replica_evidence};

    if request.fault_type.acts_on_callers() {
        return route_network_fault(state, auth, headers, request, self_name).await;
    }
    let namespace = request.namespace.clone().unwrap_or_default();
    let (statuses, faults) = tokio::join!(
        collect_cluster_statuses(state, FAULT_EVIDENCE_TIMEOUT),
        collect_cluster_faults(state, FAULT_EVIDENCE_TIMEOUT),
    );
    // A peer that didn't answer contributes no replicas, which only makes the
    // replica rail stricter.
    let statuses = match statuses {
        Ok((statuses, _unreachable)) => statuses,
        Err(error) => return unavailable_response(error),
    };
    let instances: Vec<WorkloadInstance> = statuses
        .into_iter()
        .filter(|status| {
            status.instance.app_name == request.target_service
                && status.instance.namespace == namespace
        })
        .map(|status| WorkloadInstance {
            running: status.instance.state == "running",
            node: status.node,
            instance_id: status.instance.id,
        })
        .collect();
    let evidence = replica_evidence(&request, &instances, &faults.0);

    let context = crate::smoker::types::SafetyContext {
        // Workload faults only meet the replica rail; zeroed cluster fields
        // make the node rails stand aside, as they do in standalone mode.
        council_size: 0,
        council_nodes_with_active_faults: 0,
        leader_node_id: String::new(),
        total_nodes: 0,
        nodes_with_active_faults: 0,
        target_service_replicas: evidence.replicas,
        target_service_faulted_replicas: evidence.faulted_replicas,
    };
    let check = crate::smoker::safety::evaluate_safety(&request, &context);
    if !check.approved {
        let reason = check
            .violation
            .map(|violation| violation.to_string())
            .unwrap_or_else(|| "safety check failed".to_string());
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": reason })),
        )
            .into_response();
    }

    let plan = match plan_workload_fault(&request, &instances) {
        Ok(plan) => plan,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    };

    send_routed_faults(state, auth, headers, plan, self_name, Some(evidence)).await
}

/// Route a network fault to the nodes that run its callers.
///
/// Network faults act where a connection starts, so a destination-wide fault
/// goes to every live node and a `--from` fault to the nodes that run the
/// source app in the fault's namespace. No replica rail applies: nothing is
/// stopped, only traffic towards the target changes.
pub(super) async fn route_network_fault(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    headers: &HeaderMap,
    request: crate::smoker::types::FaultRequest,
    self_name: &str,
) -> Response {
    use crate::smoker::routing::{WorkloadInstance, plan_network_fault};

    let namespace = request.namespace.clone().unwrap_or_default();
    let mut nodes: Vec<String> = match &state.membership {
        Some(membership) => membership
            .read()
            .await
            .iter()
            .map(|member| member.node_id.0.clone())
            .collect(),
        None => Vec::new(),
    };
    nodes.push(self_name.to_string());
    let sources: Vec<WorkloadInstance> = match request.fault_type.source_app() {
        Some(source) => match collect_cluster_statuses(state, FAULT_EVIDENCE_TIMEOUT).await {
            Ok((statuses, _unreachable)) => statuses
                .into_iter()
                .filter(|status| {
                    status.instance.app_name == source && status.instance.namespace == namespace
                })
                .map(|status| WorkloadInstance {
                    running: status.instance.state == "running",
                    node: status.node,
                    instance_id: status.instance.id,
                })
                .collect(),
            Err(error) => return unavailable_response(error),
        },
        None => Vec::new(),
    };
    let plan = match plan_network_fault(&request, &nodes, &sources) {
        Ok(plan) => plan,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    };
    send_routed_faults(state, auth, headers, plan, self_name, None).await
}

/// Apply this node's share of a routed fault and forward every other share,
/// returning one summary whose `routed` lists the rest.
pub(super) async fn send_routed_faults(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    headers: &HeaderMap,
    plan: Vec<crate::smoker::routing::RoutedFault>,
    self_name: &str,
    evidence: Option<crate::smoker::types::ReplicaEvidence>,
) -> Response {
    let mut applied: Vec<crate::smoker::types::FaultSummary> = Vec::new();
    for routed in plan {
        let result = if routed.node == self_name {
            apply_fault_locally(state, auth, routed.request, None, evidence).await
        } else {
            forward_workload_fault(state, &routed.node, headers, &routed.request).await
        };
        match result {
            Ok(mut summary) => {
                summary.node = Some(routed.node);
                applied.push(summary);
            }
            Err(response) if applied.is_empty() => return response,
            Err(response) => {
                return partial_fault_response(&routed.node, response, applied).await;
            }
        }
    }
    let mut applied = applied.into_iter();
    let Some(mut first) = applied.next() else {
        return (StatusCode::BAD_REQUEST, "fault matched no instances").into_response();
    };
    first.routed = applied.collect();
    Json(first).into_response()
}

/// Forward one owner's share of a workload fault and read back its summary.
// `Response` is large but it IS the HTTP reply to send on failure;
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
pub(super) async fn forward_workload_fault(
    state: &ApiState,
    node: &str,
    headers: &HeaderMap,
    request: &crate::smoker::types::FaultRequest,
) -> Result<crate::smoker::types::FaultSummary, Response> {
    let response = forward_node_fault(state, node, headers, request).await;
    if !response.status().is_success() {
        return Err(response);
    }
    let body = axum::body::to_bytes(response.into_body(), MAX_FAULT_FORWARD_RESPONSE_BYTES)
        .await
        .map_err(|error| {
            (
                StatusCode::BAD_GATEWAY,
                format!("failed to read fault response from {node}: {error}"),
            )
                .into_response()
        })?;
    serde_json::from_slice(&body).map_err(|error| {
        (
            StatusCode::BAD_GATEWAY,
            format!("node {node} returned an unreadable fault summary: {error}"),
        )
            .into_response()
    })
}

/// A routed fault took effect on some owners and failed on another. Report
/// both, so the operator can clear what did land.
pub(super) async fn partial_fault_response(
    failed_node: &str,
    failure: Response,
    applied: Vec<crate::smoker::types::FaultSummary>,
) -> Response {
    let status = failure.status();
    let body = axum::body::to_bytes(failure.into_body(), MAX_FAULT_FORWARD_RESPONSE_BYTES)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let landed: Vec<String> = applied
        .iter()
        .map(|summary| {
            format!(
                "{} on {}",
                summary.id,
                summary.node.as_deref().unwrap_or("?")
            )
        })
        .collect();
    (
        status,
        Json(serde_json::json!({
            "error": format!(
                "fault failed on {failed_node} ({body}) after it took effect as {}",
                landed.join(", ")
            ),
            "applied": applied,
        })),
    )
        .into_response()
}

/// Every node's active faults, each tagged with the node that holds it, plus
/// one message per peer that didn't answer.
pub(super) async fn collect_cluster_faults(
    state: &ApiState,
    peer_timeout: std::time::Duration,
) -> (Vec<crate::smoker::types::FaultSummary>, Vec<String>) {
    let local_name = local_node_name(state);
    let mut failures = Vec::new();
    let mut faults: Vec<_> = match ask_agent(&state.cmd_tx, |response| AgentCommand::ListFaults {
        response,
    })
    .await
    {
        Ok(local) => local
            .into_iter()
            .map(|mut fault| {
                fault.node = Some(local_name.clone());
                fault
            })
            .collect(),
        Err(_) => {
            failures.push(format!("node {local_name}: agent unavailable"));
            Vec::new()
        }
    };
    let members = match &state.membership {
        Some(membership) => membership.read().await.clone(),
        None => Vec::new(),
    };
    let requests = futures_util::stream::iter(
        members
            .into_iter()
            .filter(|member| member.node_id.0 != local_name)
            .map(|member| async move {
                let name = member.node_id.0;
                let result = tokio::time::timeout(peer_timeout, async {
                    let url = state
                        .cluster_http
                        .url(&member.address.to_string(), "/v1/fault");
                    let mut request = state.cluster_http.client().get(url);
                    if let Some(token) = &state.service_token {
                        request = request.bearer_auth(token);
                    }
                    request
                        .send()
                        .await?
                        .error_for_status()?
                        .json::<Vec<crate::smoker::types::FaultSummary>>()
                        .await
                })
                .await;
                match result {
                    Ok(Ok(faults)) => Ok(faults
                        .into_iter()
                        .map(|mut fault| {
                            fault.node = Some(name.clone());
                            fault
                        })
                        .collect::<Vec<_>>()),
                    Ok(Err(error)) => Err(format!("node {name}: {error}")),
                    Err(_) => Err(format!("node {name} timed out")),
                }
            }),
    )
    .buffer_unordered(8);
    tokio::pin!(requests);
    while let Some(result) = requests.next().await {
        match result {
            Ok(node_faults) => faults.extend(node_faults),
            Err(failure) => failures.push(failure),
        }
    }
    failures.sort();
    faults.sort_by(|left, right| (&left.node, left.id).cmp(&(&right.node, right.id)));
    (faults, failures)
}

/// Complete a forwarded node request with one deadline and a bounded body.
pub(super) async fn send_node_request(
    target_node: &str,
    request: reqwest::RequestBuilder,
    operation: &str,
) -> Response {
    let deadline = tokio::time::Instant::now() + NODE_REQUEST_TIMEOUT;
    let response = match tokio::time::timeout_at(deadline, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("failed to forward node {operation} to {target_node}: {error}"),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                format!("node {operation} request to {target_node} timed out"),
            )
                .into_response();
        }
    };
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if response
        .content_length()
        .is_some_and(|length| length > MAX_FAULT_FORWARD_RESPONSE_BYTES as u64)
    {
        return (
            StatusCode::BAD_GATEWAY,
            "target node response exceeded the 64 KiB limit",
        )
            .into_response();
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = match tokio::time::timeout_at(deadline, stream.next()).await {
        Ok(chunk) => chunk,
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                format!("node {operation} response from {target_node} timed out"),
            )
                .into_response();
        }
    } {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("failed to read node {operation} response from {target_node}: {error}"),
                )
                    .into_response();
            }
        };
        if body.len().saturating_add(chunk.len()) > MAX_FAULT_FORWARD_RESPONSE_BYTES {
            return (
                StatusCode::BAD_GATEWAY,
                "target node response exceeded the 64 KiB limit",
            )
                .into_response();
        }
        body.extend_from_slice(&chunk);
    }
    (status, body).into_response()
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct FaultClearQuery {
    pub(super) node: Option<String>,
    #[serde(default)]
    pub(super) acknowledged: bool,
}

/// Clear a specific fault by ID.
pub(super) async fn fault_clear_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    known: Option<axum::Extension<KnownMembers>>,
    headers: HeaderMap,
    Path(id): Path<u64>,
    Query(query): Query<FaultClearQuery>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_user(
        auth.as_deref(),
        crate::sesame::types::ApiRole::Deployer,
    ) {
        return resp;
    }
    let (principal, role) = auth
        .as_deref()
        .map(|auth| (auth.principal_id.as_str(), auth.role))
        .unwrap_or(("local-bootstrap", crate::sesame::types::ApiRole::Admin));
    let caller = crate::testkit::safety::OperationAuthorisation {
        principal,
        role,
        acknowledged: query.acknowledged,
    };
    let allow_workload_fault = state
        .static_capabilities
        .test_policy
        .authorise_reversal(
            crate::testkit::safety::OperationPermission::InjectWorkloadFaults,
            &caller,
        )
        .is_ok();
    let allow_node_pressure = state
        .static_capabilities
        .test_policy
        .authorise_reversal(
            crate::testkit::safety::OperationPermission::SaturateCapacity,
            &caller,
        )
        .is_ok();
    let allow_node_fault =
        if let Some(target_node) = query.node.as_deref().filter(|target| !target.is_empty()) {
            // Node routing is not itself authority. Preserve the three independent
            // reversal grants and let the owning agent inspect the actual fault
            // before it removes anything.
            let allow_node_fault = state
                .static_capabilities
                .test_policy
                .authorise_reversal(
                    crate::testkit::safety::OperationPermission::AlterNodeState,
                    &caller,
                )
                .is_ok();
            if state
                .node_name
                .as_deref()
                .is_some_and(|name| name != target_node)
            {
                return forward_node_fault_clear(
                    &state,
                    known.as_deref(),
                    target_node,
                    &headers,
                    id,
                    query.acknowledged,
                )
                .await;
            }
            allow_node_fault
        } else {
            false
        };
    let has_any_reversal_grant = allow_workload_fault || allow_node_fault || allow_node_pressure;
    if query.node.is_some() && !has_any_reversal_grant {
        return (
            StatusCode::FORBIDDEN,
            "cluster policy does not allow reversal of this fault class",
        )
            .into_response();
    }
    if query.node.is_none() && !allow_workload_fault {
        return (
            StatusCode::FORBIDDEN,
            "workload fault reversal requires inject_workload_faults authorisation",
        )
            .into_response();
    }
    // One budget covers the agent's answer and the release wait, so a node
    // that forwarded this clear hears this node's own verdict, not its own
    // deadline passing.
    let deadline = tokio::time::Instant::now() + NODE_FAULT_CLEAR_BUDGET;
    let cleared = ask_agent(&state.cmd_tx, |response| AgentCommand::ClearFault {
        fault_id: id,
        allow_workload_fault,
        allow_node_fault,
        allow_node_pressure,
        response,
    });
    let Ok(cleared) = tokio::time::timeout_at(deadline, cleared).await else {
        // A clear already queued still runs; asking again is idempotent.
        return (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({
                "error": format!(
                    "the agent has not answered the clear of fault {id} yet; retry the clear"
                )
            })),
        )
            .into_response();
    };
    match cleared {
        Ok(Ok(clearance)) => {
            if let Some(sequence) = clearance.reservation
                && !wait_for_node_fault_release(&state, sequence, deadline).await
            {
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(serde_json::json!({
                        "error": format!(
                            "fault {id} is reversed on this node, but the cluster has not yet \
                             released its reservation; retry the clear before injecting again"
                        )
                    })),
                )
                    .into_response();
            }
            record_fault_audit(
                &state,
                FaultAudit {
                    action: "fault.cleared",
                    principal,
                    severity: crate::bun::events::EventSeverity::Info,
                    app: None,
                    node: query.node,
                    details: std::collections::BTreeMap::from([(
                        "fault_id".to_string(),
                        id.to_string(),
                    )]),
                    message: format!("fault {id} cleared by principal {principal}"),
                },
            )
            .await;
            Json(serde_json::json!({ "message": clearance.message })).into_response()
        }
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(response) => response,
    }
}

/// How long a node that forwards a node-level request waits for the owning
/// node's answer.
pub(super) const NODE_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a clear may spend on the owning node: the agent's answer plus the
/// wait for the council to release a node fault's reservation. It stays a
/// second under [`NODE_REQUEST_TIMEOUT`], so a forwarded clear reports this
/// node's own verdict rather than the forwarder's timeout.
pub(super) const NODE_FAULT_CLEAR_BUDGET: std::time::Duration =
    NODE_REQUEST_TIMEOUT.saturating_sub(std::time::Duration::from_secs(1));

/// Wait until the council no longer holds the reservation a cleared node fault
/// owned, or `deadline` passes. Returns whether it was released.
///
/// The leader's reaper releases a reservation only after it has fenced the
/// target node through its own live membership view. So once this returns
/// `true`, the leader that will judge the next node fault has already seen
/// this node back, and the single experiment slot is free again.
pub(super) async fn wait_for_node_fault_release(
    state: &ApiState,
    sequence: u64,
    deadline: tokio::time::Instant,
) -> bool {
    let Some(council) = &state.council else {
        return true;
    };
    loop {
        let released = council
            .desired_state()
            .await
            .node_fault_reservations
            .active
            .is_none_or(|grant| grant.sequence != sequence);
        if released {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Route manual reversal to the node which owns the local fault id.
pub(super) async fn forward_node_fault_clear(
    state: &ApiState,
    known: Option<&KnownMembers>,
    target_node: &str,
    headers: &HeaderMap,
    fault_id: u64,
    acknowledged: bool,
) -> Response {
    let path = format!("/v1/fault/{fault_id}");
    // A node-killed target is dead to gossip but still holds its fault.
    let url = match known_node_api_url(state, known, target_node, &path).await {
        Ok(url) => url,
        Err(response) => return response,
    };
    let forwarded = state.cluster_http.client().delete(url).query(&[
        ("node", target_node),
        ("acknowledged", if acknowledged { "true" } else { "false" }),
    ]);
    send_node_request(
        target_node,
        copy_forwarded_auth(forwarded, headers),
        "fault reversal",
    )
    .await
}

/// Clear all active faults.
pub(super) async fn fault_clear_all_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_user(
        auth.as_deref(),
        crate::sesame::types::ApiRole::Deployer,
    ) {
        return resp;
    }
    let (principal, role) = auth
        .as_deref()
        .map(|auth| (auth.principal_id.as_str(), auth.role))
        .unwrap_or(("local-bootstrap", crate::sesame::types::ApiRole::Admin));
    if let Err(error) = state.static_capabilities.test_policy.authorise_reversal(
        crate::testkit::safety::OperationPermission::InjectWorkloadFaults,
        &crate::testkit::safety::OperationAuthorisation {
            principal,
            role,
            acknowledged: false,
        },
    ) {
        return (StatusCode::FORBIDDEN, error.to_string()).into_response();
    }
    // `?service=NAME` clears only that service's faults; no query clears all
    // workload faults. An *empty* `?service=` is neither: every node-class
    // fault carries an empty `target_service`, so it would match them all —
    // reject it rather than let this Deployer-authorised path reverse Admin
    // faults by omission.
    let target = match params.get("service") {
        Some(service) if service.is_empty() => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "service must be non-empty; omit ?service to clear all workload faults"
                })),
            )
                .into_response();
        }
        Some(service) => match params.get("namespace") {
            // Confined clear: scope-check the named namespace, so a Deployer
            // scoped to one tenant cannot reverse another tenant's same-named
            // service faults (AUTH1).
            Some(namespace) => {
                if let Err(response) =
                    crate::sesame::auth::authorize_scoped(auth.as_deref(), service, namespace)
                {
                    return response;
                }
                Some((service.clone(), Some(namespace.clone())))
            }
            // Cross-namespace clear: reversing a service's faults in every
            // namespace is a cluster-wide action, so a scoped token is refused
            // and told to name a namespace it may touch (C3, as for reads).
            None => {
                if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
                    return response;
                }
                Some((service.clone(), None))
            }
        },
        None => None,
    };
    let command = |response| match target {
        Some((service, namespace)) => AgentCommand::ClearFaultsByService {
            service,
            namespace,
            response,
        },
        None => AgentCommand::ClearAllFaults { response },
    };
    match ask_agent(&state.cmd_tx, command).await {
        Ok(Ok(msg)) => {
            // Workload faults are routed to the nodes that run their targets,
            // so a clear has to reach those nodes too. Peers get `local=true`
            // and the caller's own credential, so each repeats every check.
            let msg = if params.get("local").is_some_and(|local| local == "true") {
                msg
            } else {
                let mut messages = vec![msg];
                messages.extend(clear_faults_on_peers(&state, &headers, &params).await);
                messages.join("; ")
            };
            let service = params.get("service").cloned();
            let mut details = std::collections::BTreeMap::new();
            let action = if let Some(service) = &service {
                details.insert("target_service".to_string(), service.clone());
                "fault.cleared-by-service"
            } else {
                "fault.cleared-all-workload"
            };
            record_fault_audit(
                &state,
                FaultAudit {
                    action,
                    principal,
                    severity: crate::bun::events::EventSeverity::Info,
                    app: service,
                    node: None,
                    details,
                    message: format!("{msg} by principal {principal}"),
                },
            )
            .await;
            Json(serde_json::json!({ "message": msg })).into_response()
        }
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(response) => response,
    }
}

/// Send a clear-all or clear-by-service to every other live member and
/// describe each answer. A peer that can't be reached is reported, not fatal:
/// its faults still expire on their own.
pub(super) async fn clear_faults_on_peers(
    state: &ApiState,
    headers: &HeaderMap,
    params: &std::collections::HashMap<String, String>,
) -> Vec<String> {
    let local_name = local_node_name(state);
    let members = match &state.membership {
        Some(membership) => membership.read().await.clone(),
        None => return Vec::new(),
    };
    let mut query: Vec<(&str, &str)> = params
        .iter()
        .filter(|(key, _)| matches!(key.as_str(), "service" | "namespace"))
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    query.push(("local", "true"));
    let mut messages = Vec::new();
    for member in members
        .into_iter()
        .filter(|member| member.node_id.0 != local_name)
    {
        let node = member.node_id.0;
        let url = state
            .cluster_http
            .url(&member.address.to_string(), "/v1/fault");
        let request = state.cluster_http.client().delete(url).query(&query);
        let response =
            send_node_request(&node, copy_forwarded_auth(request, headers), "fault clear").await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), MAX_FAULT_FORWARD_RESPONSE_BYTES)
            .await
            .unwrap_or_default();
        let text = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value["message"].as_str().map(str::to_string))
            .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
        messages.push(if status.is_success() {
            format!("{node}: {text}")
        } else {
            format!("{node}: not cleared ({status}): {text}")
        });
    }
    messages
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct FaultListQuery {
    #[serde(default)]
    pub(super) cluster: bool,
}

/// Every node's active faults, as `GET /v1/fault?cluster=true` returns them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterFaultList {
    /// Active faults, each tagged with the node that holds it.
    pub faults: Vec<crate::smoker::types::FaultSummary>,
    /// One message per node whose faults couldn't be read.
    pub warnings: Vec<String>,
}

/// List active faults: this node's by default, every node's with
/// `?cluster=true`.
pub(super) async fn fault_list_handler(
    State(state): State<ApiState>,
    Query(query): Query<FaultListQuery>,
) -> Response {
    if query.cluster {
        let (faults, warnings) = collect_cluster_faults(&state, CLUSTER_STATUS_TIMEOUT).await;
        return Json(ClusterFaultList { faults, warnings }).into_response();
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::ListFaults {
        response,
    })
    .await
    {
        Ok(summaries) => Json(serde_json::json!(summaries)).into_response(),
        Err(response) => response,
    }
}
