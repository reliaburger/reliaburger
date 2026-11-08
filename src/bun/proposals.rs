//! The leader's side of agent proposals (#1682): judging and applying an
//! approval, and the reaper that expires, compacts and auto-approves.
//!
//! Approval and application are one Raft entry, `ProposalApply`. Before
//! writing it the leader judges the proposal against the policies in force
//! (`proposal::guard`), on a blocking thread, and stamps the entry with the
//! policy generation it judged at. If a policy was written in between, the
//! state machine answers `PolicyGenerationMoved` and the leader judges
//! again, as it does for a fenced app write (chapter 18).
//!
//! The reaper runs on the leader only, like the lease reaper, and resumes
//! on a new leader because every proposal lives in the council.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::bun::events::{AuditEvent, EventKind, EventSeverity, EventStore};
use crate::council::{CouncilError, CouncilNode, CouncilResponse, RaftRequest};
use crate::proposal::guard::{GuardInput, GuardStage, GuardVerdict, judge_proposal};
use crate::proposal::{
    Approval, FailedReason, Principal, Proposal, ProposalId, ProposalOutcome, ProposalRefusal,
};
use crate::sesame::types::{ApiRole, TokenKind};

/// How often the leader looks for proposals to expire, compact or
/// auto-approve. The shortest TTL is five minutes, so a second is noise.
pub const PROPOSAL_REAPER_INTERVAL: Duration = Duration::from_secs(1);

/// How many times the leader judges again after the policies moved under
/// it before giving up.
const MAX_JUDGEMENTS: usize = 5;

/// The principal the reaper's records name: the cluster itself.
pub const REAPER_PRINCIPAL: &str = "system";

/// The caller a policy approval is judged as: the cluster.
pub fn system_principal() -> Principal {
    Principal {
        name: REAPER_PRINCIPAL.to_string(),
        principal_id: REAPER_PRINCIPAL.to_string(),
        kind: TokenKind::Human,
        role: ApiRole::Admin,
    }
}

/// Judge `proposal` on a blocking thread.
pub async fn judge(
    proposal: Proposal,
    caller: Principal,
    stage: GuardStage,
    policies: std::collections::BTreeMap<String, crate::config::PolicySpec>,
) -> GuardVerdict {
    tokio::task::spawn_blocking(move || {
        judge_proposal(&GuardInput {
            proposal: &proposal,
            caller: &caller,
            stage,
            policies: &policies,
        })
    })
    .await
    .unwrap_or_else(|error| GuardVerdict {
        refusal: Some(crate::proposal::guard::GuardRefusal {
            rule: "policy".to_string(),
            message: format!("the policies couldn't be evaluated: {error}"),
        }),
        ..GuardVerdict::default()
    })
}

/// Approve proposal `id` as `approval`, judging it as `caller` first, on
/// this node, which must lead. Answers what the `ProposalApply` entry did.
pub async fn approve(
    council: &CouncilNode,
    id: ProposalId,
    approval: Approval,
    caller: Principal,
) -> Result<ProposalOutcome, CouncilError> {
    for _ in 0..MAX_JUDGEMENTS {
        let (proposal, policies, generation) = council
            .read_desired(|desired| {
                (
                    desired.proposals.get(id).cloned(),
                    desired.policies.clone(),
                    desired.policy_generation,
                )
            })
            .await;
        let refusal = match proposal {
            Some(proposal) if proposal.state.is_pending() => {
                judge(proposal, caller.clone(), GuardStage::Approve, policies)
                    .await
                    .refusal
                    .map(|refusal| FailedReason::Guardrail {
                        rule: refusal.rule,
                        message: refusal.message,
                    })
            }
            // The state machine answers a missing or decided proposal.
            _ => None,
        };
        let request = RaftRequest::ProposalApply {
            id,
            approval: approval.clone(),
            policy_generation: generation,
            refusal,
            observed_at_unix_ms: crate::testkit::lease::now_unix_millis(),
        };
        match council.write(request).await? {
            CouncilResponse::Proposal(outcome) => return Ok(*outcome),
            CouncilResponse::PolicyGenerationMoved { .. } => continue,
            other => return Ok(unexpected(other)),
        }
    }
    Ok(ProposalOutcome::Refused {
        refusal: ProposalRefusal::Invalid {
            reason: "the policies kept changing while the approval was judged; try again"
                .to_string(),
        },
    })
}

/// Put applied proposal `id` back, for a person, judging the `before`
/// specs by the app policies as `relish apply` would.
pub async fn undo(
    council: &CouncilNode,
    id: ProposalId,
    by: Principal,
) -> Result<ProposalOutcome, CouncilError> {
    for _ in 0..MAX_JUDGEMENTS {
        let (proposal, policies, generation) = council
            .read_desired(|desired| {
                (
                    desired.proposals.get(id).cloned(),
                    desired.policies.clone(),
                    desired.policy_generation,
                )
            })
            .await;
        if let Some(proposal) = proposal {
            let caller = by.clone();
            let verdict = tokio::task::spawn_blocking(move || {
                let specs: Vec<_> = proposal
                    .changes
                    .iter()
                    .filter_map(|change| {
                        let spec = change.before.spec.as_deref()?;
                        Some((change.app.clone(), spec.clone()))
                    })
                    .collect();
                crate::proposal::guard::judge_specs(
                    &policies,
                    &caller,
                    specs.iter().map(|(app, spec)| (app, spec)),
                )
            })
            .await
            .unwrap_or_default();
            if let Some(refusal) = verdict.refusal {
                return Ok(ProposalOutcome::Refused {
                    refusal: ProposalRefusal::Guardrail {
                        rule: refusal.rule,
                        message: refusal.message,
                    },
                });
            }
        }
        let request = RaftRequest::ProposalUndo {
            id,
            by: by.clone(),
            policy_generation: generation,
            observed_at_unix_ms: crate::testkit::lease::now_unix_millis(),
        };
        match council.write(request).await? {
            CouncilResponse::Proposal(outcome) => return Ok(*outcome),
            CouncilResponse::PolicyGenerationMoved { .. } => continue,
            other => return Ok(unexpected(other)),
        }
    }
    Ok(ProposalOutcome::Refused {
        refusal: ProposalRefusal::Invalid {
            reason: "the policies kept changing while the undo was judged; try again".to_string(),
        },
    })
}

fn unexpected(other: CouncilResponse) -> ProposalOutcome {
    let reason = match other {
        CouncilResponse::Refused { reason } => reason,
        other => format!("unexpected council answer: {other:?}"),
    };
    ProposalOutcome::Refused {
        refusal: ProposalRefusal::Invalid { reason },
    }
}

/// The audit record for `action` on `proposal`, by `principal`.
pub fn proposal_event(
    action: &str,
    principal: &str,
    proposal: &Proposal,
    node: &str,
    now_unix_ms: u64,
    message: String,
) -> AuditEvent {
    let severity = match action {
        "proposal.failed" | "proposal.refused" | "proposal.undone" => EventSeverity::Warning,
        _ => EventSeverity::Info,
    };
    AuditEvent {
        timestamp: now_unix_ms / 1000,
        kind: EventKind::Operation,
        severity,
        action: action.to_string(),
        principal: principal.to_string(),
        app: (proposal.changes.len() == 1).then(|| proposal.changes[0].app.name.clone()),
        namespace: Some(proposal.namespace.clone()),
        node: Some(node.to_string()),
        source_address: None,
        details: proposal.audit_details(),
        message,
    }
}

async fn record(events: Option<&Arc<RwLock<EventStore>>>, records: Vec<AuditEvent>) {
    let Some(events) = events else {
        return;
    };
    // Release the event ring's lock before waiting for the disk.
    let receipts: Vec<_> = {
        let mut store = events.write().await;
        records
            .into_iter()
            .map(|record| store.record_audit(record))
            .collect()
    };
    for receipt in receipts {
        if !receipt.written().await {
            eprintln!("bun: a proposal audit record is not on disk");
        }
    }
}

/// One reaper pass, if this node leads: expire pending proposals past their
/// TTL, compact what can't be undone any more, and let approve policies
/// approve what they match.
pub async fn reap_once(
    council: &CouncilNode,
    events: Option<&Arc<RwLock<EventStore>>>,
    node: &str,
    now_unix_ms: u64,
) -> Result<(), CouncilError> {
    if !council.is_leader().await {
        return Ok(());
    }
    let (needs_reaping, any_expired) = council
        .read_desired(|desired| {
            (
                desired.proposals.needs_reaping(now_unix_ms),
                desired
                    .proposals
                    .pending()
                    .any(|proposal| proposal.expires_at_unix_ms <= now_unix_ms),
            )
        })
        .await;
    if any_expired
        && let CouncilResponse::Proposal(outcome) = council
            .write(RaftRequest::ProposalExpire { now_unix_ms })
            .await?
        && let ProposalOutcome::Expired { proposals } = *outcome
    {
        let records = proposals
            .iter()
            .map(|proposal| {
                proposal_event(
                    "proposal.expired",
                    REAPER_PRINCIPAL,
                    proposal,
                    node,
                    now_unix_ms,
                    format!(
                        "proposal {} by {} expired before anyone decided",
                        proposal.id, proposal.author.name
                    ),
                )
            })
            .collect();
        record(events, records).await;
    }
    if needs_reaping {
        council
            .write(RaftRequest::ProposalCompact { now_unix_ms })
            .await?;
    }
    auto_approve_pending(council, events, node).await
}

/// Judge every pending proposal against the approve policies, and apply
/// the ones a policy approves. Does nothing while no policy can approve
/// (`guard::may_auto_approve`).
pub async fn auto_approve_pending(
    council: &CouncilNode,
    events: Option<&Arc<RwLock<EventStore>>>,
    node: &str,
) -> Result<(), CouncilError> {
    let (pending, policies) = council
        .read_desired(|desired| {
            if !crate::proposal::guard::may_auto_approve(&desired.policies) {
                return (Vec::new(), Default::default());
            }
            (
                desired.proposals.pending().cloned().collect::<Vec<_>>(),
                desired.policies.clone(),
            )
        })
        .await;
    for proposal in pending {
        let verdict = judge(
            proposal.clone(),
            system_principal(),
            GuardStage::Approve,
            policies.clone(),
        )
        .await;
        let Some(name) = verdict.approve else {
            continue;
        };
        let outcome = approve(
            council,
            proposal.id,
            Approval::Policy { name: name.clone() },
            system_principal(),
        )
        .await?;
        let principal = format!("policy:{name}");
        if let Some(event) = outcome_event(&outcome, &principal, node) {
            record(events, vec![event]).await;
        }
    }
    Ok(())
}

/// The audit record an approval's outcome gets, if it changed anything.
pub fn outcome_event(outcome: &ProposalOutcome, principal: &str, node: &str) -> Option<AuditEvent> {
    let now = crate::testkit::lease::now_unix_millis();
    let (action, proposal, message) = match outcome {
        ProposalOutcome::Applied { proposal } => (
            "proposal.applied",
            proposal,
            format!(
                "{principal} approved proposal {} by {}; it applied",
                proposal.id, proposal.author.name
            ),
        ),
        ProposalOutcome::Failed { proposal } => (
            "proposal.failed",
            proposal,
            format!(
                "{principal} approved proposal {} by {}; nothing was written",
                proposal.id, proposal.author.name
            ),
        ),
        _ => return None,
    };
    let mut event = proposal_event(action, principal, proposal, node, now, message);
    if principal.starts_with("policy:") {
        // The approver was an admin's policy, not a token.
        event
            .details
            .insert("principal_kind".to_string(), "policy".to_string());
    }
    Some(event)
}

/// Run [`reap_once`] every [`PROPOSAL_REAPER_INTERVAL`] until `shutdown`.
/// Followers tick too, and do nothing.
pub fn spawn_proposal_reaper(
    council: Arc<CouncilNode>,
    events: Option<Arc<RwLock<EventStore>>>,
    node: String,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PROPOSAL_REAPER_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = ticker.tick() => {}
            }
            let now = crate::testkit::lease::now_unix_millis();
            if let Err(error) = reap_once(&council, events.as_ref(), &node, now).await {
                eprintln!("bun: proposal reaping deferred: {error}");
            }
        }
    })
}
