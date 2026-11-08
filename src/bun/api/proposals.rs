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
pub(super) async fn proposal_create_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
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
        _ => crate::config::PermissionAction::Deploy,
    };
    let named = match crate::proposal::build::requested_apps(&request) {
        Ok(apps) => apps,
        Err(refusal) => return refusal_response(&refusal, None),
    };
    if let Err(response) = authorise_apps(&state, auth, action, named.iter()).await {
        return response;
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
    let built = {
        let author = author.clone();
        let request = request.clone();
        tokio::task::spawn_blocking(move || {
            let policies = desired.policies.clone();
            let built =
                crate::proposal::build::build(&desired, &request, &author, now, |app, spec| {
                    crate::proposal::guard::fill_defaults(&policies, &author, app, spec)
                });
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
    // An undo's apps come from the original; the caller must cover them.
    if let Err(response) = authorise_apps(&state, auth, action, proposal.apps()).await {
        return response;
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
            "{} proposed {} ({})",
            author.name,
            created.id,
            created.summary.first().cloned().unwrap_or_default()
        ),
    )
    .await;
    // A policy approved it on the spot.
    if let Some(name) = verdict.approve {
        let approval = Approval::Policy { name: name.clone() };
        let outcome = match crate::bun::proposals::approve(
            council,
            created.id,
            approval,
            crate::bun::proposals::system_principal(),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(error) => return council_unavailable(error),
        };
        if let Some(event) = crate::bun::proposals::outcome_event(
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

/// `POST /v1/proposals/{id}/approve`: a person approves, and the leader
/// applies it in the same Raft entry. The same approver asking again gets
/// the outcome.
pub(super) async fn proposal_approve_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    Path(id): Path<u64>,
    headers: HeaderMap,
) -> Response {
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
        && by.name == approver.name
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
    let outcome = match crate::bun::proposals::approve(
        council,
        id,
        Approval::Human(approver.clone()),
        approver.clone(),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => return council_unavailable(error),
    };
    answer_outcome(&state, auth, outcome, &approver).await
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
    authorise_apps(state, auth, action, proposal.apps()).await?;
    if auth.is_some_and(|auth| auth.token_name == proposal.author.name) {
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
    if matches!(proposal.state, crate::proposal::ProposalState::Failed { .. }) {
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
        crate::proposal::FailedReason::Guardrail { rule, message } => {
            ProposalRefusal::Guardrail {
                rule: rule.clone(),
                message: message.clone(),
            }
        }
        crate::proposal::FailedReason::Refused { reason } => ProposalRefusal::WrongState {
            id: proposal.id,
            state: format!("failed: {reason}"),
        },
    };
    refusal_response(&refusal, Some(view))
}

/// The audit record for `action` on `proposal`, by the caller.
async fn record_proposal(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    action: &str,
    proposal: &Proposal,
    message: String,
) {
    let event = crate::bun::proposals::proposal_event(
        action,
        "",
        proposal,
        &local_node_name(state),
        crate::testkit::lease::now_unix_millis(),
        message,
    );
    let mut details = event.details;
    details.insert(
        "principal_kind".to_string(),
        auth.map_or("human", |auth| auth.kind.as_str()).to_string(),
    );
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
        (
            "principal_kind".to_string(),
            auth.map_or("human", |auth| auth.kind.as_str()).to_string(),
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
