//! The proposal routes (#1682): propose a change, read proposals, and
//! approve, reject, withdraw or undo one.
//!
//! Every write forwards to the leader, which builds the proposal from its
//! own desired state, judges it against the policies, and writes one Raft
//! entry. Approval and application are that one entry, so a retried
//! approval after a leader change finds the outcome rather than running
//! twice.
//!
//! Who may: any token reads, within its scope. A deployer or above, of
//! either kind, proposes; an agent token's granted role counts here and on
//! the lease routes only. Approving, rejecting and undoing are a person's:
//! a human token, deployer or above, whose scope and `[permission]` cover
//! every touched app, and never the author. Withdrawing is the author's.

use super::*;

use crate::proposal::view::{ProposalListing, ProposalView, RejectRequest};
use crate::proposal::{
    Approval, Principal, Proposal, ProposalId, ProposalOutcome, ProposalRefusal, ProposalRequest,
    RequestKind,
};

/// `?dry_run=true` on `POST /v1/proposals`.
#[derive(Debug, Default, serde::Deserialize)]
pub(super) struct ProposeQuery {
    #[serde(default)]
    dry_run: bool,
}

/// The caller as a proposal principal. Before any token exists, the
/// node's bootstrap caller, which counts as a person.
pub(crate) fn principal_of(auth: Option<&crate::sesame::auth::AuthContext>) -> Principal {
    match auth {
        Some(auth) => Principal {
            name: auth.token_name.clone(),
            principal_id: auth.principal_id.clone(),
            kind: auth.kind,
            role: auth.granted_role,
        },
        None => Principal {
            name: "local-bootstrap".to_string(),
            principal_id: "local-bootstrap".to_string(),
            kind: crate::sesame::types::TokenKind::Human,
            role: crate::sesame::types::ApiRole::Admin,
        },
    }
}

/// Whether `auth`'s scope reaches every app `proposal` touches.
fn visible(auth: Option<&crate::sesame::auth::AuthContext>, proposal: &Proposal) -> bool {
    proposal
        .apps()
        .all(|app| crate::sesame::auth::authorize_scoped(auth, &app.name, &app.namespace).is_ok())
}

/// `GET /v1/proposals`: every proposal the caller's scope reaches, newest
/// first.
pub(super) async fn proposals_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(response) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return response;
    }
    let Some(council) = &state.council else {
        return Json(ProposalListing::default()).into_response();
    };
    let now = crate::testkit::lease::now_unix_millis();
    let auth = auth.as_deref();
    let listing = council
        .read_desired(|desired| ProposalListing {
            proposals: desired
                .proposals
                .proposals
                .values()
                .rev()
                .filter(|proposal| visible(auth, proposal))
                .map(|proposal| proposal.view(now))
                .collect(),
        })
        .await;
    Json(listing).into_response()
}

/// `GET /v1/proposals/{id}`: one proposal, or 404 when it doesn't exist or
/// is outside the caller's scope.
pub(super) async fn proposal_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(id): Path<u64>,
) -> Response {
    if let Err(response) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return response;
    }
    match read_proposal(&state, auth.as_deref(), ProposalId(id)).await {
        Ok(proposal) => {
            Json(proposal.view(crate::testkit::lease::now_unix_millis())).into_response()
        }
        Err(response) => response,
    }
}

#[allow(clippy::result_large_err)]
async fn read_proposal(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    id: ProposalId,
) -> Result<Proposal, Response> {
    let Some(council) = &state.council else {
        return Err(standalone());
    };
    let proposal = council
        .read_desired(|desired| desired.proposals.get(id).cloned())
        .await;
    match proposal {
        Some(proposal) if visible(auth, &proposal) => Ok(proposal),
        _ => Err(refusal_response(&ProposalRefusal::NotFound { id }, None)),
    }
}

fn standalone() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": "proposals live in the cluster's council; this node is standalone",
            "code": "invalid",
        })),
    )
        .into_response()
}

/// Scope and `[permission]` over every app, with `action`.
#[allow(clippy::result_large_err)]
async fn authorise_apps<'a>(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    action: crate::config::PermissionAction,
    apps: impl Iterator<Item = &'a crate::meat::AppId>,
) -> Result<(), Response> {
    for app in apps {
        crate::sesame::auth::authorize_scoped(auth, &app.name, &app.namespace)?;
        enforce_permission(state, auth, action, &app.name, &app.namespace).await?;
    }
    Ok(())
}

/// `POST /v1/proposals`: propose a change. `?dry_run=true` answers the
/// whole proposal without storing it.
#[allow(clippy::too_many_arguments)]
pub(super) async fn proposal_create_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    capacity: Option<axum::Extension<crate::cluster::capacity::CapacityAdmission>>,
    Query(query): Query<ProposeQuery>,
    headers: HeaderMap,
    Json(request): Json<ProposalRequest>,
) -> Response {
    let auth = auth.as_deref();
    if let Err(response) =
        crate::sesame::auth::authorize_proposer(auth, crate::sesame::types::ApiRole::Deployer)
    {
        return response;
    }
    let action = match request.kind {
        RequestKind::Scale => crate::config::PermissionAction::Scale,
        RequestKind::ClearFault => crate::config::PermissionAction::Fault,
        _ => crate::config::PermissionAction::Deploy,
    };
    let named = match crate::proposal::build::requested_apps(&request) {
        Ok(apps) => apps,
        Err(refusal) => return refusal_response(&refusal, None),
    };
    if let Err(response) = authorise_apps(&state, auth, action, named.iter()).await {
        return response;
    }
    // An undo names no app; its apps are the original's. Check them before
    // building it, whose refusals name them, and answer an original out of
    // scope as a missing one, as reading it would.
    if request.kind == RequestKind::Undo
        && let Some(of) = request.of
    {
        let original = match read_proposal(&state, auth, ProposalId(of)).await {
            Ok(original) => original,
            Err(response) => return response,
        };
        if let Err(response) = authorise_apps(&state, auth, action, original.apps()).await {
            return response;
        }
    }
    let Some(council) = &state.council else {
        return standalone();
    };
    let body = serde_json::to_string(&request).unwrap_or_default();
    let path = if query.dry_run {
        "/v1/proposals?dry_run=true"
    } else {
        "/v1/proposals"
    };
    if let Some(forwarded) = super::upgrade::forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        path,
        &headers,
        &body,
    )
    .await
    {
        return forwarded;
    }
    let author = principal_of(auth);
    let now = crate::testkit::lease::now_unix_millis();
    let desired = council.desired_state().await;
    let fault = match request.kind {
        RequestKind::ClearFault => match find_fault(&state, &request).await {
            Ok(fault) => Some(fault),
            Err(refusal) => {
                record_refusal(&state, auth, &request, &refusal).await;
                return refusal_response(&refusal, None);
            }
        },
        _ => None,
    };
    let context = crate::proposal::build::BuildContext {
        history_target: rollback_history_target(&state, &desired, &request).await,
        fault,
    };
    let built = {
        let author = author.clone();
        let request = request.clone();
        tokio::task::spawn_blocking(move || {
            let policies = desired.policies.clone();
            let built = crate::proposal::build::build_with_context(
                &desired,
                &request,
                &author,
                now,
                |app, spec| crate::proposal::guard::fill_defaults(&policies, &author, app, spec),
                &context,
            );
            (built, policies)
        })
        .await
    };
    let (built, policies) = match built {
        Ok(result) => result,
        Err(error) => {
            return refusal_response(
                &ProposalRefusal::Invalid {
                    reason: format!("the proposal couldn't be built: {error}"),
                },
                None,
            );
        }
    };
    let mut proposal = match built {
        Ok(proposal) => proposal,
        Err(refusal) => {
            record_refusal(&state, auth, &request, &refusal).await;
            return refusal_response(&refusal, None);
        }
    };
    // An undo's apps come from the original, and a clear_fault's from the
    // fault; the caller must cover them.
    if let Err(response) = authorise_apps(&state, auth, action, proposal.apps()).await {
        return response;
    }
    if let Some(forecast) = forecast(capacity.as_deref(), &proposal).await {
        crate::proposal::build::with_forecast(&mut proposal, forecast);
    }
    let verdict = crate::bun::proposals::judge(
        proposal.clone(),
        author.clone(),
        crate::proposal::guard::GuardStage::Create,
        policies,
    )
    .await;
    if let Some(refusal) = verdict.refusal {
        let refusal = ProposalRefusal::Guardrail {
            rule: refusal.rule,
            message: refusal.message,
        };
        record_refusal(&state, auth, &request, &refusal).await;
        return refusal_response(&refusal, None);
    }
    proposal.findings = verdict.findings;
    if query.dry_run {
        return Json(proposal.view(now)).into_response();
    }
    let outcome = match council
        .write(crate::council::types::RaftRequest::ProposalCreate {
            proposal: Box::new(proposal),
            observed_at_unix_ms: now,
        })
        .await
    {
        Ok(crate::council::types::CouncilResponse::Proposal(outcome)) => *outcome,
        Ok(other) => return unexpected(other),
        Err(error) => return council_unavailable(error),
    };
    let created = match outcome {
        ProposalOutcome::Created { proposal } => proposal,
        ProposalOutcome::Existing { proposal } => {
            return Json(proposal.view(now)).into_response();
        }
        ProposalOutcome::Refused { refusal } => {
            record_refusal(&state, auth, &request, &refusal).await;
            return refusal_response(&refusal, None);
        }
        other => return unexpected_outcome(&other),
    };
    record_proposal(
        &state,
        auth,
        "proposal.created",
        &created,
        format!(
            "{} proposed {} ({}){}",
            author.name,
            created.id,
            created.summary.first().cloned().unwrap_or_default(),
            // Built-in rule 3 kept it for a person although a policy
            // matched: say so, so nobody wonders why it waits.
            verdict
                .held
                .as_deref()
                .map(|held| format!("; {held}"))
                .unwrap_or_default()
        ),
    )
    .await;
    // A policy approved it on the spot.
    if let Some(name) = verdict.approve {
        let approval = Approval::Policy { name: name.clone() };
        let outcome = match crate::bun::proposals::approve(council, created.id, approval).await {
            Ok(outcome) => outcome,
            Err(error) => return council_unavailable(error),
        };
        for event in crate::bun::proposals::policy_outcome_events(
            &outcome,
            &format!("policy:{name}"),
            &local_node_name(&state),
        ) {
            write_audit(&state, event).await;
        }
        if let ProposalOutcome::Applied { proposal } | ProposalOutcome::Failed { proposal } =
            outcome
        {
            return (StatusCode::CREATED, Json(proposal.view(now))).into_response();
        }
    }
    (StatusCode::CREATED, Json(created.view(now))).into_response()
}

/// For a proposed rollback the council has no released spec for, the
/// target `relish rollback` would take from the nodes' deploy history.
/// `None` for every other request, and when the council has one: asking
/// every node is only worth it when the cheap answer is missing.
async fn rollback_history_target(
    state: &ApiState,
    desired: &crate::council::types::DesiredState,
    request: &ProposalRequest,
) -> Option<crate::config::app::AppSpec> {
    if request.kind != RequestKind::Rollback {
        return None;
    }
    let namespace = request.namespace.as_deref().unwrap_or("default");
    let app = crate::meat::AppId::new(request.app.as_deref()?, namespace);
    let current = desired.apps.get(&app)?;
    let released = desired
        .rollouts
        .get(&app)
        .and_then(|rollouts| crate::bun::cluster_view::released_rollback_target(rollouts, current));
    if released.is_some() {
        return None;
    }
    super::deploys::history_rollback_target(state, &app.name, namespace, Some(current)).await
}

/// `POST /v1/proposals/{id}/approve`: a person approves, and the leader
/// applies it in the same Raft entry. The same approver asking again gets
/// the outcome.
pub(super) async fn proposal_approve_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    known: Option<axum::Extension<KnownMembers>>,
    Path(id): Path<u64>,
    headers: HeaderMap,
) -> Response {
    let auth_extension = auth.clone();
    let auth = auth.as_deref();
    let id = ProposalId(id);
    let proposal =
        match authorise_decider(&state, auth, id, crate::config::PermissionAction::Deploy).await {
            Ok(proposal) => proposal,
            Err(response) => return response,
        };
    let approver = principal_of(auth);
    // Idempotent: a retry after a leader change finds the outcome.
    if let crate::proposal::ProposalState::Applied {
        approval: Approval::Human(by),
        ..
    }
    | crate::proposal::ProposalState::Failed {
        approval: Approval::Human(by),
        ..
    } = &proposal.state
        && by.is_credential(&approver)
    {
        return answer_decided(&proposal);
    }
    let Some(council) = &state.council else {
        return standalone();
    };
    if let Some(forwarded) = super::upgrade::forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        &format!("/v1/proposals/{id}/approve"),
        &headers,
        "",
    )
    .await
    {
        return forwarded;
    }
    let approval = Approval::Human(approver.clone());
    let outcome = match &proposal.fault {
        // The fault is cleared on its node, as the approver, with the
        // checks `relish fault clear` makes, before the entry records it.
        Some(fault) => {
            let clear = clear_fault_action(
                state.clone(),
                auth_extension,
                known,
                headers.clone(),
                fault.clone(),
            );
            crate::bun::proposals::approve_with(council, id, approval, Some(clear)).await
        }
        None => crate::bun::proposals::approve(council, id, approval).await,
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => return council_unavailable(error),
    };
    answer_outcome(&state, auth, outcome, &approver).await
}

/// Clear `fault` on its node as the approver, through the same handler as
/// `relish fault clear ID --node NODE`: the approver's reversal grant,
/// scope and `[permission]`, the owning node's own checks, and, for a fault
/// that reserved node-chaos capacity, the wait for the council to release
/// it. `None` when it's cleared; otherwise why not.
fn clear_fault_action(
    state: ApiState,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    known: Option<axum::Extension<KnownMembers>>,
    headers: HeaderMap,
    fault: crate::proposal::FaultTarget,
) -> impl FnOnce() -> std::pin::Pin<
    Box<dyn std::future::Future<Output = crate::bun::proposals::ActionResult> + Send>,
> {
    move || {
        Box::pin(async move {
            // A node's fault ids restart at 1 when its bun restarts, so the
            // id may now name another fault. Clear it only if it's still
            // the one the proposal showed.
            let (listed, _) =
                super::faults::collect_cluster_faults(&state, CLUSTER_STATUS_TIMEOUT).await;
            if let Err(reason) = fault_still_matches(&fault, &listed) {
                return Some(crate::proposal::FailedReason::Fault { reason });
            }
            let response = super::faults::fault_clear_handler(
                auth,
                State(state),
                known,
                headers,
                Path(fault.id),
                Query(super::faults::FaultClearQuery {
                    node: Some(fault.node.clone()),
                    acknowledged: false,
                }),
            )
            .await;
            if response.status().is_success() {
                return None;
            }
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap_or_default();
            let said = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value["error"].as_str().map(str::to_string))
                .unwrap_or_else(|| String::from_utf8_lossy(&body).trim().to_string());
            Some(crate::proposal::FailedReason::Fault {
                reason: format!(
                    "fault {} on node {} wasn't cleared ({status}): {said}",
                    fault.id, fault.node
                ),
            })
        })
    }
}

/// Whether `listed` still holds the fault a `clear_fault` proposal
/// recorded: the same id on the same node, on the same app, instance and
/// kind of fault. A node's fault registry isn't persisted and its ids
/// restart at 1, so after a restart the id can name a fault nobody
/// proposed clearing; that one stays active.
pub(crate) fn fault_still_matches(
    recorded: &crate::proposal::FaultTarget,
    listed: &[crate::smoker::types::FaultSummary],
) -> Result<(), String> {
    let Some(found) = listed.iter().find(|fault| {
        fault.id == recorded.id
            && fault
                .node
                .as_deref()
                .is_none_or(|node| node == recorded.node)
    }) else {
        return Err(format!(
            "fault {} on node {} is gone: it expired, or someone cleared it",
            recorded.id, recorded.node
        ));
    };
    let same = found.target_service == recorded.app.name
        && found.namespace.as_deref() == Some(recorded.app.namespace.as_str())
        && found.target_instance == recorded.instance
        && found.fault_type == recorded.fault_type;
    if same {
        return Ok(());
    }
    Err(format!(
        "fault {} on node {} is now {} on {}, not the {} on {} the proposal showed; it wasn't \
         cleared (the node's fault ids restart after it restarts)",
        recorded.id,
        recorded.node,
        found.fault_type,
        found.namespace.as_deref().map_or_else(
            || found.target_service.clone(),
            |namespace| { format!("{namespace}/{}", found.target_service) }
        ),
        recorded.fault_type,
        recorded.app
    ))
}

/// The fault `request` names, found in every node's list: by id, and by
/// node when two nodes hold the same id. Only a fault on an app may be
/// proposed; a node fault changes a node, and a person clears it.
async fn find_fault(
    state: &ApiState,
    request: &ProposalRequest,
) -> Result<crate::proposal::FaultTarget, ProposalRefusal> {
    let invalid = |reason: String| ProposalRefusal::Invalid { reason };
    let Some(id) = request.fault else {
        return Err(invalid(
            "a clear_fault names the fault in `fault`, its id in `relish fault list`".to_string(),
        ));
    };
    let (faults, warnings) =
        super::faults::collect_cluster_faults(state, CLUSTER_STATUS_TIMEOUT).await;
    let matching: Vec<_> = faults
        .into_iter()
        .filter(|fault| fault.id == id)
        .filter(|fault| {
            request
                .node
                .as_deref()
                .is_none_or(|node| fault.node.as_deref() == Some(node))
        })
        .collect();
    let fault = match matching.as_slice() {
        [fault] => fault,
        [] => {
            let unread = if warnings.is_empty() {
                String::new()
            } else {
                format!(" (some nodes didn't answer: {})", warnings.join("; "))
            };
            return Err(invalid(format!(
                "there's no active fault {id}{}{unread}",
                request
                    .node
                    .as_deref()
                    .map(|node| format!(" on node {node}"))
                    .unwrap_or_default()
            )));
        }
        several => {
            let nodes: Vec<&str> = several
                .iter()
                .filter_map(|fault| fault.node.as_deref())
                .collect();
            return Err(invalid(format!(
                "nodes {} each hold a fault {id}; name one with `node`",
                nodes.join(", ")
            )));
        }
    };
    let (Some(namespace), false) = (fault.namespace.as_deref(), fault.target_service.is_empty())
    else {
        return Err(ProposalRefusal::Guardrail {
            rule: "node_fault".to_string(),
            message: format!(
                "fault {id} is a node fault; it changes a node, not an app, so a person clears \
                 it with `relish fault clear {id}`"
            ),
        });
    };
    Ok(crate::proposal::FaultTarget {
        id,
        node: fault.node.clone().unwrap_or_else(|| local_node_name(state)),
        app: crate::meat::AppId::new(&fault.target_service, namespace),
        fault_type: fault.fault_type.clone(),
        instance: fault.target_instance.clone(),
        remaining_secs: fault.remaining_secs,
        injected_by: fault.injected_by.clone(),
    })
}

/// The scheduler's dry run for `proposal`, when it starts instances
/// (#1682): asked of this node's scheduling loop, which runs on the
/// leader. With no scheduler here, or one that can't answer yet, the
/// forecast says why instead.
async fn forecast(
    capacity: Option<&crate::cluster::capacity::CapacityAdmission>,
    proposal: &Proposal,
) -> Option<crate::proposal::build::Forecast> {
    if proposal.blast_radius.instances_started == 0 {
        return None;
    }
    let Some(capacity) = capacity else {
        return Some(crate::proposal::build::Forecast {
            note: Some("this node runs no scheduler".to_string()),
            ..Default::default()
        });
    };
    let apps = proposal
        .changes
        .iter()
        .map(|change| crate::cluster::capacity::ForecastApp {
            app_id: change.app.clone(),
            spec: change.after.spec.as_deref().cloned(),
            stopped: change.after.stopped,
        })
        .collect();
    match capacity.forecast(apps).await {
        Ok(apps) => {
            let mut forecast = crate::proposal::build::Forecast::default();
            let mut reasons = Vec::new();
            for (app, placed) in apps {
                forecast
                    .nodes
                    .extend(placed.nodes.into_iter().map(|node| node.0));
                forecast.unplaced += placed.unplaced;
                if let Some(reason) = placed.reason {
                    reasons.push(if proposal.changes.len() == 1 {
                        reason
                    } else {
                        format!("{app}: {reason}")
                    });
                }
            }
            if !reasons.is_empty() {
                forecast.note = Some(reasons.join("; "));
            }
            Some(forecast)
        }
        Err(error) => Some(crate::proposal::build::Forecast {
            note: Some(match error {
                crate::cluster::capacity::CapacityAdmissionError::Unavailable(why) => why,
                other => other.to_string(),
            }),
            ..Default::default()
        }),
    }
}

/// `POST /v1/proposals/{id}/reject`: a person says no.
pub(super) async fn proposal_reject_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    Path(id): Path<u64>,
    headers: HeaderMap,
    Json(request): Json<RejectRequest>,
) -> Response {
    let auth = auth.as_deref();
    let id = ProposalId(id);
    if let Err(response) =
        authorise_decider(&state, auth, id, crate::config::PermissionAction::Deploy).await
    {
        return response;
    }
    let by = principal_of(auth);
    let body = serde_json::to_string(&request).unwrap_or_default();
    let reason = request.reason.clone();
    proposal_write(
        &state,
        auth,
        directory.as_deref(),
        &headers,
        &format!("/v1/proposals/{id}/reject"),
        &body,
        &by.clone(),
        move |now| crate::council::types::RaftRequest::ProposalReject {
            id,
            by,
            reason,
            observed_at_unix_ms: now,
        },
    )
    .await
}

/// `POST /v1/proposals/{id}/withdraw`: the author takes it back.
pub(super) async fn proposal_withdraw_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    Path(id): Path<u64>,
    headers: HeaderMap,
) -> Response {
    let auth = auth.as_deref();
    if let Err(response) =
        crate::sesame::auth::authorize_proposer(auth, crate::sesame::types::ApiRole::Deployer)
    {
        return response;
    }
    let id = ProposalId(id);
    if let Err(response) = read_proposal(&state, auth, id).await {
        return response;
    }
    let by = principal_of(auth);
    proposal_write(
        &state,
        auth,
        directory.as_deref(),
        &headers,
        &format!("/v1/proposals/{id}/withdraw"),
        "",
        &by.clone(),
        move |now| crate::council::types::RaftRequest::ProposalWithdraw {
            id,
            by,
            observed_at_unix_ms: now,
        },
    )
    .await
}

/// `POST /v1/proposals/{id}/undo`: a person puts an applied proposal back.
/// Agents propose an undo instead (`kind: undo`).
pub(super) async fn proposal_undo_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    Path(id): Path<u64>,
    headers: HeaderMap,
) -> Response {
    let auth = auth.as_deref();
    if let Err(response) =
        crate::sesame::auth::authorize_user(auth, crate::sesame::types::ApiRole::Deployer)
    {
        return response;
    }
    let id = ProposalId(id);
    let proposal = match read_proposal(&state, auth, id).await {
        Ok(proposal) => proposal,
        Err(response) => return response,
    };
    if let Err(response) = authorise_apps(
        &state,
        auth,
        crate::config::PermissionAction::Deploy,
        proposal.apps(),
    )
    .await
    {
        return response;
    }
    let Some(council) = &state.council else {
        return standalone();
    };
    if let Some(forwarded) = super::upgrade::forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        &format!("/v1/proposals/{id}/undo"),
        &headers,
        "",
    )
    .await
    {
        return forwarded;
    }
    let by = principal_of(auth);
    let outcome = match crate::bun::proposals::undo(council, id, by.clone()).await {
        Ok(outcome) => outcome,
        Err(error) => return council_unavailable(error),
    };
    answer_outcome(&state, auth, outcome, &by).await
}

/// A person who may decide proposal `id`: a human token (an agent's
/// effective role is read-only, so the role check refuses it), deployer
/// or above, covering every touched app. The state machine checks again
/// that it isn't the author.
#[allow(clippy::result_large_err)]
async fn authorise_decider(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    id: ProposalId,
    action: crate::config::PermissionAction,
) -> Result<Proposal, Response> {
    if let Some(auth) = auth
        && auth.kind == crate::sesame::types::TokenKind::Agent
    {
        return Err(refusal_response(
            &ProposalRefusal::AgentDecides {
                action: "approve or reject".to_string(),
            },
            None,
        ));
    }
    crate::sesame::auth::authorize_user(auth, crate::sesame::types::ApiRole::Deployer)?;
    let proposal = read_proposal(state, auth, id).await?;
    // Clearing a fault is a fault permission, as `relish fault clear` is.
    let action = match proposal.kind {
        crate::proposal::ProposalKind::ClearFault => crate::config::PermissionAction::Fault,
        _ => action,
    };
    authorise_apps(state, auth, action, proposal.apps()).await?;
    // By name too: a rotated token, or one made again under the same
    // name, is still its author here.
    if auth.is_some() && proposal.author.may_be(&principal_of(auth)) {
        return Err(refusal_response(
            &ProposalRefusal::SelfApproval {
                id,
                name: proposal.author.name.clone(),
            },
            None,
        ));
    }
    Ok(proposal)
}

/// Forward to the leader, or write `build(now)` here, then audit and
/// answer.
#[allow(clippy::too_many_arguments)]
async fn proposal_write(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    directory: Option<&LeaderDirectory>,
    headers: &HeaderMap,
    path: &str,
    body: &str,
    by: &Principal,
    build: impl FnOnce(u64) -> crate::council::types::RaftRequest,
) -> Response {
    let Some(council) = &state.council else {
        return standalone();
    };
    if let Some(forwarded) =
        super::upgrade::forward_upgrade_to_leader(state, council, directory, path, headers, body)
            .await
    {
        return forwarded;
    }
    let now = crate::testkit::lease::now_unix_millis();
    let outcome = match council.write(build(now)).await {
        Ok(crate::council::types::CouncilResponse::Proposal(outcome)) => *outcome,
        Ok(other) => return unexpected(other),
        Err(error) => return council_unavailable(error),
    };
    answer_outcome(state, auth, outcome, by).await
}

/// Audit what an entry did and answer it.
async fn answer_outcome(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    outcome: ProposalOutcome,
    by: &Principal,
) -> Response {
    let now = crate::testkit::lease::now_unix_millis();
    let (action, proposal, message) = match outcome {
        ProposalOutcome::Applied { proposal } => (
            "proposal.applied",
            proposal,
            format!("{} approved proposal {{}}; it applied", by.name),
        ),
        ProposalOutcome::Failed { proposal } => (
            "proposal.failed",
            proposal,
            format!("{} approved proposal {{}}; nothing was written", by.name),
        ),
        ProposalOutcome::Rejected { proposal } => (
            "proposal.rejected",
            proposal,
            format!("{} rejected proposal {{}}", by.name),
        ),
        ProposalOutcome::Withdrawn { proposal } => (
            "proposal.withdrawn",
            proposal,
            format!("{} withdrew proposal {{}}", by.name),
        ),
        ProposalOutcome::Undone { proposal } => (
            "proposal.undone",
            proposal,
            format!("{} undid proposal {{}}", by.name),
        ),
        ProposalOutcome::Refused { refusal } => {
            return refusal_response(&refusal, None);
        }
        other => return unexpected_outcome(&other),
    };
    let message = message.replace("{}", &proposal.id.to_string());
    record_proposal(state, auth, action, &proposal, message).await;
    let direction = match action {
        "proposal.applied" => Some(crate::bun::proposals::AppChangeDirection::Apply),
        "proposal.undone" => Some(crate::bun::proposals::AppChangeDirection::Undo),
        _ => None,
    };
    if let Some(direction) = direction {
        record_app_changes(state, auth, &proposal, direction).await;
    }
    if matches!(
        proposal.state,
        crate::proposal::ProposalState::Failed { .. }
    ) {
        return answer_decided(&proposal);
    }
    Json(proposal.view(now)).into_response()
}

/// The answer for a proposal that's already decided: its view, or, when
/// its approval wrote nothing, the refusal that says why (`stale`,
/// `lease_held` or `guardrail`) with the view beside it. An approval that
/// changed nothing isn't a success, so a script reading the status or an
/// agent reading the code doesn't take it for one.
fn answer_decided(proposal: &Proposal) -> Response {
    let view = proposal.view(crate::testkit::lease::now_unix_millis());
    let crate::proposal::ProposalState::Failed { why, .. } = &proposal.state else {
        return Json(view).into_response();
    };
    let refusal = match why {
        crate::proposal::FailedReason::Stale { app, .. } => ProposalRefusal::Stale {
            app: app.to_string(),
            what: "after the proposal was made, so it wasn't applied; propose again".to_string(),
        },
        crate::proposal::FailedReason::Lease { lease } => ProposalRefusal::LeaseHeld {
            lease: lease.clone(),
        },
        crate::proposal::FailedReason::Guardrail { rule, message } => ProposalRefusal::Guardrail {
            rule: rule.clone(),
            message: message.clone(),
        },
        crate::proposal::FailedReason::BlastRadiusGrew { what } => {
            ProposalRefusal::BlastRadiusGrew { what: what.clone() }
        }
        crate::proposal::FailedReason::Refused { reason }
        | crate::proposal::FailedReason::Fault { reason } => ProposalRefusal::WrongState {
            id: proposal.id,
            state: format!("failed: {reason}"),
        },
    };
    refusal_response(&refusal, Some(view))
}

/// The app records a person's approval or undo of `proposal` wrote, one
/// per app it changed, attributed to that person and naming the proposal,
/// its author and the author's reason.
async fn record_app_changes(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    proposal: &Proposal,
    direction: crate::bun::proposals::AppChangeDirection,
) {
    let (principal, _) = audit_principal(auth);
    let events = crate::bun::proposals::app_change_events(
        proposal,
        direction,
        &principal,
        super::audit_principal_kind(auth),
        &local_node_name(state),
        crate::testkit::lease::now_unix_millis(),
    );
    for event in events {
        record_caller_action(
            state,
            auth,
            CallerAction {
                kind: event.kind,
                severity: event.severity,
                action: &event.action,
                app: event.app,
                namespace: event.namespace,
                node: None,
                details: event.details,
                message: event.message,
            },
        )
        .await;
    }
}

/// The audit record for `action` on `proposal`, by the caller.
async fn record_proposal(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    action: &str,
    proposal: &Proposal,
    message: String,
) {
    // The principal and its kind come from the caller, below.
    let event = crate::bun::proposals::proposal_event(
        action,
        "",
        crate::sesame::types::PrincipalKind::Unknown,
        proposal,
        &local_node_name(state),
        crate::testkit::lease::now_unix_millis(),
        message,
    );
    let details = event.details;
    record_caller_action(
        state,
        auth,
        CallerAction {
            kind: event.kind,
            severity: event.severity,
            action,
            app: event.app,
            namespace: event.namespace,
            node: None,
            details,
            message: event.message,
        },
    )
    .await;
}

/// The `proposal.refused` record: who asked for what, and the rule that
/// refused it.
async fn record_refusal(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    request: &ProposalRequest,
    refusal: &ProposalRefusal,
) {
    let (_, name) = audit_principal(auth);
    let mut details = std::collections::BTreeMap::from([
        (
            "kind".to_string(),
            serde_json::to_value(request.kind)
                .ok()
                .and_then(|kind| kind.as_str().map(str::to_string))
                .unwrap_or_default(),
        ),
        ("code".to_string(), refusal.code().to_string()),
        ("message".to_string(), refusal.to_string()),
        (
            "reason".to_string(),
            crate::proposal::build::clean_reason(&request.reason),
        ),
    ]);
    if let ProposalRefusal::Guardrail { rule, .. } = refusal {
        details.insert("rule".to_string(), rule.clone());
    }
    record_caller_action(
        state,
        auth,
        CallerAction {
            kind: crate::bun::events::EventKind::Operation,
            severity: crate::bun::events::EventSeverity::Warning,
            action: "proposal.refused",
            app: request.app.clone(),
            namespace: Some(
                request
                    .namespace
                    .clone()
                    .unwrap_or_else(|| "default".to_string()),
            ),
            node: None,
            details,
            message: format!("{name}'s proposal was refused: {refusal}"),
        },
    )
    .await;
}

/// The HTTP answer for a refusal, with a `code` an agent can branch on.
fn refusal_response(refusal: &ProposalRefusal, view: Option<ProposalView>) -> Response {
    let status = match refusal {
        ProposalRefusal::NotFound { .. } => StatusCode::NOT_FOUND,
        ProposalRefusal::NotAuthor { .. }
        | ProposalRefusal::SelfApproval { .. }
        | ProposalRefusal::AgentDecides { .. }
        | ProposalRefusal::Guardrail { .. } => StatusCode::FORBIDDEN,
        ProposalRefusal::Invalid { .. } => StatusCode::BAD_REQUEST,
        ProposalRefusal::WrongState { .. }
        | ProposalRefusal::Conflict { .. }
        | ProposalRefusal::LeaseHeld { .. }
        | ProposalRefusal::Stale { .. }
        | ProposalRefusal::BlastRadiusGrew { .. }
        | ProposalRefusal::NoUndo { .. }
        | ProposalRefusal::Limit { .. } => StatusCode::CONFLICT,
    };
    let mut body = serde_json::json!({ "error": refusal.to_string(), "code": refusal.code() });
    match refusal {
        ProposalRefusal::LeaseHeld { lease } => {
            body["lease"] =
                serde_json::to_value(lease.view(crate::testkit::lease::now_unix_millis()))
                    .unwrap_or_default();
        }
        ProposalRefusal::Conflict { proposal, .. } => {
            body["proposal"] = serde_json::json!(proposal.0);
        }
        ProposalRefusal::Guardrail { rule, .. } => {
            body["rule"] = serde_json::json!(rule);
        }
        _ => {}
    }
    if let Some(view) = view {
        body["view"] = serde_json::to_value(view).unwrap_or_default();
    }
    (status, Json(body)).into_response()
}

fn council_unavailable(error: crate::council::CouncilError) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": format!("could not update the cluster: {error}"),
            "code": "council_unavailable",
        })),
    )
        .into_response()
}

fn unexpected(other: crate::council::types::CouncilResponse) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": format!("unexpected council answer: {other:?}") })),
    )
        .into_response()
}

fn unexpected_outcome(outcome: &ProposalOutcome) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": format!("unexpected proposal outcome: {outcome:?}") })),
    )
        .into_response()
}
