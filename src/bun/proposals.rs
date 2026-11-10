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
    AppRevision, Approval, FailedReason, Principal, Proposal, ProposalId, ProposalOutcome,
    ProposalRefusal,
};
use crate::sesame::types::PrincipalKind;

/// How often the leader looks for proposals to expire, compact or
/// auto-approve. The shortest TTL is five minutes, so a second is noise.
pub const PROPOSAL_REAPER_INTERVAL: Duration = Duration::from_secs(1);

/// How many times the leader judges again after the policies moved under
/// it before giving up.
const MAX_JUDGEMENTS: usize = 5;

/// The principal the reaper's records name: the cluster itself.
pub const REAPER_PRINCIPAL: &str = "system";

/// Who an approval of `proposal` is judged as: the person who approves,
/// or, for a policy approval, the proposal's author. Nobody stands behind
/// a policy approval but the author, so a policy that judges by `caller`
/// sees the agent that asked, not an admin.
pub fn judged_as(proposal: &Proposal, approval: &Approval) -> Principal {
    match approval {
        Approval::Human(by) => by.clone(),
        Approval::Policy { .. } => proposal.author.clone(),
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

/// Approve proposal `id` as `approval`, judging it first as
/// [`judged_as`] says, on this node, which must lead. Answers what the
/// `ProposalApply` entry did.
pub async fn approve(
    council: &CouncilNode,
    id: ProposalId,
    approval: Approval,
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
                let caller = judged_as(&proposal, &approval);
                judge(proposal, caller, GuardStage::Approve, policies)
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

/// The audit record for `action` on `proposal`, by `principal`, a
/// principal of `principal_kind`.
pub fn proposal_event(
    action: &str,
    principal: &str,
    principal_kind: PrincipalKind,
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
        principal_kind,
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
                    PrincipalKind::System,
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
        // Judged as its author: a policy approval has nobody else behind it.
        let verdict = judge(
            proposal.clone(),
            proposal.author.clone(),
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
        )
        .await?;
        let principal = format!("policy:{name}");
        record(events, policy_outcome_events(&outcome, &principal, node)).await;
    }
    Ok(())
}

/// The audit records a policy's approval writes: the proposal's own, and,
/// when it applied, one per app it changed ([`app_change_events`]). A
/// policy is never a token, so its records are written here rather than
/// through the API's caller attribution.
pub fn policy_outcome_events(
    outcome: &ProposalOutcome,
    principal: &str,
    node: &str,
) -> Vec<AuditEvent> {
    let Some(event) = outcome_event(outcome, principal, node) else {
        return Vec::new();
    };
    let mut events = vec![event];
    if let ProposalOutcome::Applied { proposal } = outcome {
        events.extend(app_change_events(
            proposal,
            AppChangeDirection::Apply,
            principal,
            PrincipalKind::Policy,
            node,
            crate::testkit::lease::now_unix_millis(),
        ));
    }
    events
}

/// The audit record a policy's approval outcome gets, if it changed
/// anything.
fn outcome_event(outcome: &ProposalOutcome, principal: &str, node: &str) -> Option<AuditEvent> {
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
    Some(proposal_event(
        action,
        principal,
        PrincipalKind::Policy,
        proposal,
        node,
        now,
        message,
    ))
}

/// Which way a proposal's changes went: applied, or undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppChangeDirection {
    /// From each change's `before` to its `after`.
    Apply,
    /// Back from `after` to `before`.
    Undo,
}

/// What a change did to its app, named as the direct route would name it:
/// `app.deleted` when the spec went away, `app.stopped` when the app
/// stopped, and `app.applied` for everything else (a new spec, a scale, a
/// rollback, a start).
pub fn app_change_action(before: &AppRevision, after: &AppRevision) -> &'static str {
    if before.spec.is_some() && after.spec.is_none() {
        "app.deleted"
    } else if after.stopped && !before.stopped {
        "app.stopped"
    } else {
        "app.applied"
    }
}

/// The facts of `proposal` an app record carries (#1682's design): the
/// proposal, its kind, its author and the author's reason, and who
/// approved it. The reason is the author's own words, unverified, as it is
/// on the proposal's own records.
fn proposal_facts(proposal: &Proposal) -> std::collections::BTreeMap<String, String> {
    const CARRIED: [&str; 7] = [
        "proposal",
        "kind",
        "author",
        "author_kind",
        "reason",
        "approved_by",
        "undo_of",
    ];
    proposal
        .audit_details()
        .into_iter()
        .filter(|(key, _)| CARRIED.contains(&key.as_str()))
        .collect()
}

/// One audit record per app `proposal` changed, as the direct route would
/// have written it, attributed to whoever approved (or undid) it and
/// naming the proposal, its author and the author's reason. So "which agent
/// made which change, and why" is answered from the change's own record,
/// not only from `proposal.applied`.
pub fn app_change_events(
    proposal: &Proposal,
    direction: AppChangeDirection,
    principal: &str,
    principal_kind: PrincipalKind,
    node: &str,
    now_unix_ms: u64,
) -> Vec<AuditEvent> {
    proposal
        .changes
        .iter()
        .map(|change| {
            let (from, to) = match direction {
                AppChangeDirection::Apply => (&change.before, &change.after),
                AppChangeDirection::Undo => (&change.after, &change.before),
            };
            let action = app_change_action(from, to);
            let mut details = proposal_facts(proposal);
            let verb = match direction {
                AppChangeDirection::Apply => "applying",
                AppChangeDirection::Undo => {
                    details.insert("undo".to_string(), "true".to_string());
                    "undoing"
                }
            };
            AuditEvent {
                timestamp: now_unix_ms / 1000,
                kind: EventKind::Operation,
                severity: EventSeverity::Info,
                action: action.to_string(),
                principal: principal.to_string(),
                principal_kind,
                app: Some(change.app.name.clone()),
                namespace: Some(change.app.namespace.clone()),
                node: Some(node.to_string()),
                source_address: None,
                details,
                message: format!(
                    "{action} {}/{} by {principal}, {verb} proposal {} by {}",
                    change.app.namespace, change.app.name, proposal.id, proposal.author.name
                ),
            }
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(replicas: u32) -> crate::config::app::AppSpec {
        toml::from_str(&format!("image = \"web:1.4\"\nreplicas = {replicas}")).unwrap()
    }

    /// A proposal's app record is named as the direct route would name the
    /// same change, both ways round for an undo.
    #[test]
    fn each_change_is_named_by_what_it_did_to_the_app() {
        let running = AppRevision::new(Some(spec(3)), false);
        let scaled = AppRevision::new(Some(spec(5)), false);
        let stopped = AppRevision::new(Some(spec(3)), true);
        let gone = AppRevision::new(None, false);
        assert_eq!(app_change_action(&running, &scaled), "app.applied");
        assert_eq!(app_change_action(&running, &stopped), "app.stopped");
        assert_eq!(app_change_action(&running, &gone), "app.deleted");
        // Undoing a stop starts the app; undoing a delete brings it back.
        assert_eq!(app_change_action(&stopped, &running), "app.applied");
        assert_eq!(app_change_action(&gone, &running), "app.applied");
    }
}
