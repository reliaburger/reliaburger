//! The proposal state machine: every rule that moves a proposal from one
//! state to another, as pure functions of the stored proposals, the stored
//! leases, each app's current revision and the clock the leader stamped in
//! the Raft entry.
//!
//! ```text
//!             ┌──────────── withdraw (author) ───────► Withdrawn
//!             │
//!  Pending ───┼──── reject (human) ──────────────────► Rejected
//!             │
//!             ├──── TTL passes (leader reaper) ──────► Expired
//!             │
//!             └──── approve (human or policy) ─┬─────► Applied ── undo ──► Undone
//!                                              └─────► Failed (stale, lease, guardrail)
//! ```
//!
//! There's no resting "approved" state: approval and application are one
//! Raft entry (`ProposalApply`), so a proposal that was approved but never
//! ran, or ran twice, can't exist. The council's state machine
//! (`council::state_machine`) calls these checks, then makes the app
//! writes itself through the same code as `AppSpec`, `AppStop` and
//! `AppDelete`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::lease::{AgentLeases, LeaseHolder, LeaseRefusal, LeaseRequest, LeaseTarget};
use super::types::*;
use crate::meat::AppId;
use crate::sesame::types::TokenKind;

/// Every proposal the council keeps, and the counter that numbers them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Proposals {
    /// Proposals by id: pending ones, applied ones inside the undo window,
    /// and up to [`MAX_TERMINAL_KEPT`] compacted records.
    pub proposals: BTreeMap<ProposalId, Proposal>,
    /// The last id handed out. Never goes down, so an id is never reused.
    pub last_id: u64,
}

/// Why `ProposalApply` won't go ahead: refused outright, changing nothing,
/// or recorded as `Failed`.
#[derive(Debug, Clone, PartialEq)]
pub enum ApplyBlock {
    /// Nothing changes: the proposal stays as it is.
    Refuse(ProposalRefusal),
    /// The proposal becomes `Failed` with this reason.
    Fail(FailedReason),
}

impl Proposals {
    /// The proposal numbered `id`.
    pub fn get(&self, id: ProposalId) -> Option<&Proposal> {
        self.proposals.get(&id)
    }

    /// Every pending proposal, in id order.
    pub fn pending(&self) -> impl Iterator<Item = &Proposal> {
        self.proposals
            .values()
            .filter(|proposal| proposal.state.is_pending())
    }

    /// The pending proposal touching `app`, if there is one.
    pub fn pending_on(&self, app: &AppId) -> Option<&Proposal> {
        self.pending()
            .find(|proposal| proposal.apps().any(|touched| touched == app))
    }

    /// Store `proposal` as a new pending proposal at `now_unix_ms`, taking
    /// and pinning a lease on each touched app for its author.
    ///
    /// `revision` answers each app's current revision. Refused, changing
    /// nothing at all, over a limit, on another pending proposal touching
    /// the same app, on an app that moved since the leader built it, or on
    /// a lease someone else holds. A retry with the author's idempotency
    /// key answers the proposal already made.
    pub fn create(
        &mut self,
        leases: &mut AgentLeases,
        mut proposal: Proposal,
        revision: impl Fn(&AppId) -> String,
        now_unix_ms: u64,
    ) -> ProposalOutcome {
        if let Some(key) = &proposal.idempotency_key
            && let Some(existing) = self.proposals.values().find(|existing| {
                existing.author.is_credential(&proposal.author)
                    && existing.idempotency_key.as_ref() == Some(key)
            })
        {
            return ProposalOutcome::Existing {
                proposal: Box::new(existing.clone()),
            };
        }
        if let Err(refusal) = self.check_create(&proposal, &revision, now_unix_ms) {
            return ProposalOutcome::Refused { refusal };
        }
        let id = ProposalId(self.last_id.saturating_add(1));
        // Taken on a copy and kept only if every app's lease is granted, so
        // a refusal on the third app leaves the first two untouched.
        let mut staged = leases.clone();
        let holder = LeaseHolder {
            name: proposal.author.name.clone(),
            principal_id: proposal.author.principal_id.clone(),
        };
        let purpose = super::lease::clean_purpose(&format!(
            "proposal {id}: {} {}",
            proposal.kind.name(),
            proposal.blast_radius.apps.join(", ")
        ));
        for app in proposal.apps() {
            let target = LeaseTarget::app(&app.namespace, &app.name);
            let request = LeaseRequest {
                target: target.clone(),
                holder: holder.clone(),
                purpose: purpose.clone(),
                ttl_ms: super::lease::DEFAULT_LEASE_TTL_MS,
            };
            let pinned = staged
                .take(&request, now_unix_ms)
                .and_then(|_| staged.pin(&target, id.0, proposal.expires_at_unix_ms, now_unix_ms));
            if let Err(refusal) = pinned {
                return ProposalOutcome::Refused {
                    refusal: lease_refusal(refusal),
                };
            }
        }
        *leases = staged;
        self.last_id = id.0;
        super::build::number(&mut proposal, id);
        proposal.state = ProposalState::Pending;
        proposal.created_at_unix_ms = now_unix_ms;
        self.proposals.insert(id, proposal.clone());
        ProposalOutcome::Created {
            proposal: Box::new(proposal),
        }
    }

    fn check_create(
        &self,
        proposal: &Proposal,
        revision: &impl Fn(&AppId) -> String,
        now_unix_ms: u64,
    ) -> Result<(), ProposalRefusal> {
        let invalid = |reason: String| Err(ProposalRefusal::Invalid { reason });
        if proposal.changes.is_empty() && proposal.fault.is_none() {
            return invalid("a proposal changes at least one app".to_string());
        }
        if proposal.changes.len() > MAX_APPS_PER_PROPOSAL {
            return Err(ProposalRefusal::Limit {
                reason: format!(
                    "a proposal touches at most {MAX_APPS_PER_PROPOSAL} apps; a bigger change is a \
                     person's `relish apply`"
                ),
            });
        }
        if let Some(app) = proposal
            .apps()
            .find(|app| app.namespace != proposal.namespace)
        {
            return invalid(format!(
                "every app in a proposal is in namespace {}, and {app} isn't",
                proposal.namespace
            ));
        }
        if proposal.reason.len() > MAX_REASON_BYTES || proposal.reason.chars().any(char::is_control)
        {
            return invalid(format!(
                "a reason is at most {MAX_REASON_BYTES} bytes with no control characters"
            ));
        }
        let ttl = proposal.expires_at_unix_ms.saturating_sub(now_unix_ms);
        if !(MIN_PROPOSAL_TTL_MS..=MAX_PROPOSAL_TTL_MS).contains(&ttl) {
            return invalid("a proposal lasts between 5 minutes and 24 hours".to_string());
        }
        let bytes = serde_json::to_vec(proposal).map_or(usize::MAX, |bytes| bytes.len());
        if bytes > MAX_PROPOSAL_BYTES {
            return Err(ProposalRefusal::Limit {
                reason: format!(
                    "the proposal is {bytes} bytes; the most is {MAX_PROPOSAL_BYTES}; split it"
                ),
            });
        }
        if self.pending().count() >= MAX_PENDING {
            return Err(ProposalRefusal::Limit {
                reason: format!(
                    "the cluster already has {MAX_PENDING} pending proposals; decide some first"
                ),
            });
        }
        let by_author = self
            .pending()
            .filter(|pending| pending.author.may_be(&proposal.author))
            .count();
        if by_author >= MAX_PENDING_PER_AUTHOR {
            return Err(ProposalRefusal::Limit {
                reason: format!(
                    "{} already has {MAX_PENDING_PER_AUTHOR} pending proposals, the most one \
                     author may",
                    proposal.author.name
                ),
            });
        }
        for app in proposal.apps() {
            if let Some(pending) = self.pending_on(app) {
                return Err(ProposalRefusal::Conflict {
                    app: app.to_string(),
                    proposal: pending.id,
                });
            }
        }
        check_revisions(
            proposal
                .changes
                .iter()
                .map(|change| (&change.app, &change.before.revision)),
            revision,
            "while the proposal was being made; propose again",
        )?;
        if let ProposalKind::Undo { of } = proposal.kind {
            self.undoable(of, now_unix_ms)?;
        }
        Ok(())
    }

    /// Whether proposal `id` may be approved by `approval` at
    /// `now_unix_ms`, and if not, whether that refuses the request or
    /// fails the proposal.
    pub fn check_apply(
        &self,
        id: ProposalId,
        approval: &Approval,
        leases: &AgentLeases,
        revision: impl Fn(&AppId) -> String,
        now_unix_ms: u64,
    ) -> Result<&Proposal, ApplyBlock> {
        let proposal = self
            .pending_proposal(id, now_unix_ms)
            .map_err(ApplyBlock::Refuse)?;
        if let Approval::Human(approver) = approval {
            check_decider(proposal, approver, "approve").map_err(ApplyBlock::Refuse)?;
        }
        if let Err(refusal) = check_revisions(
            proposal
                .changes
                .iter()
                .map(|change| (&change.app, &change.before.revision)),
            &revision,
            "after the proposal was made",
        ) {
            let ProposalRefusal::Stale { app, .. } = &refusal else {
                return Err(ApplyBlock::Refuse(refusal));
            };
            let change = proposal
                .changes
                .iter()
                .find(|change| change.app.to_string() == *app);
            return Err(ApplyBlock::Fail(FailedReason::Stale {
                app: change.map_or_else(
                    || AppId::new(app, &proposal.namespace),
                    |change| change.app.clone(),
                ),
                expected: change
                    .map(|c| c.before.revision.clone())
                    .unwrap_or_default(),
                found: change.map(|c| revision(&c.app)).unwrap_or_default(),
            }));
        }
        if let Some(lease) = proposal
            .apps()
            .find_map(|app| leases.blocker(app, &proposal.author.principal_id, now_unix_ms))
        {
            return Err(ApplyBlock::Fail(FailedReason::Lease {
                lease: Box::new(lease.clone()),
            }));
        }
        Ok(proposal)
    }

    /// Record that proposal `id` applied, ending its leases. For an undo
    /// proposal, the original becomes `Undone` too.
    pub fn mark_applied(
        &mut self,
        leases: &mut AgentLeases,
        id: ProposalId,
        approval: Approval,
        log_index: u64,
        now_unix_ms: u64,
    ) -> Option<Proposal> {
        let proposal = self.proposals.get_mut(&id)?;
        proposal.state = ProposalState::Applied {
            approval: approval.clone(),
            at_unix_ms: now_unix_ms,
            log_index,
        };
        let applied = proposal.clone();
        leases.end_pinned(id.0);
        if let ProposalKind::Undo { of } = applied.kind {
            self.mark_undone(of, approval, now_unix_ms, Some(id));
        }
        Some(applied)
    }

    /// Record that proposal `id` was approved but wrote nothing.
    pub fn mark_failed(
        &mut self,
        leases: &mut AgentLeases,
        id: ProposalId,
        approval: Approval,
        why: FailedReason,
        now_unix_ms: u64,
    ) -> Option<Proposal> {
        self.finish(
            leases,
            id,
            ProposalState::Failed {
                approval,
                why,
                at_unix_ms: now_unix_ms,
            },
        )
    }

    /// Reject pending proposal `id`. Only a person, never its author.
    pub fn reject(
        &mut self,
        leases: &mut AgentLeases,
        id: ProposalId,
        by: &Principal,
        reason: &str,
        now_unix_ms: u64,
    ) -> ProposalOutcome {
        let checked = self
            .pending_proposal(id, now_unix_ms)
            .and_then(|proposal| check_decider(proposal, by, "reject"));
        if let Err(refusal) = checked {
            return ProposalOutcome::Refused { refusal };
        }
        let state = ProposalState::Rejected {
            by: by.clone(),
            reason: super::build::clean_reason(reason),
            at_unix_ms: now_unix_ms,
        };
        match self.finish(leases, id, state) {
            Some(proposal) => ProposalOutcome::Rejected {
                proposal: Box::new(proposal),
            },
            None => ProposalOutcome::Refused {
                refusal: ProposalRefusal::NotFound { id },
            },
        }
    }

    /// Withdraw pending proposal `id`. Only its author.
    pub fn withdraw(
        &mut self,
        leases: &mut AgentLeases,
        id: ProposalId,
        by: &Principal,
        now_unix_ms: u64,
    ) -> ProposalOutcome {
        let checked = self.pending_proposal(id, now_unix_ms).and_then(|proposal| {
            if proposal.author.is_credential(by) {
                Ok(())
            } else {
                Err(ProposalRefusal::NotAuthor {
                    id,
                    author: proposal.author.name.clone(),
                })
            }
        });
        if let Err(refusal) = checked {
            return ProposalOutcome::Refused { refusal };
        }
        let state = ProposalState::Withdrawn {
            at_unix_ms: now_unix_ms,
        };
        match self.finish(leases, id, state) {
            Some(proposal) => ProposalOutcome::Withdrawn {
                proposal: Box::new(proposal),
            },
            None => ProposalOutcome::Refused {
                refusal: ProposalRefusal::NotFound { id },
            },
        }
    }

    /// Whether `by` may undo applied proposal `id` directly at
    /// `now_unix_ms`. Answers the changes to write: each app's `before`.
    pub fn check_undo(
        &self,
        id: ProposalId,
        by: &Principal,
        leases: &AgentLeases,
        revision: impl Fn(&AppId) -> String,
        now_unix_ms: u64,
    ) -> Result<&Proposal, ProposalRefusal> {
        if by.kind == TokenKind::Agent {
            return Err(ProposalRefusal::AgentDecides {
                action: "undo".to_string(),
            });
        }
        let proposal = self.undoable(id, now_unix_ms)?;
        check_revisions(
            proposal
                .changes
                .iter()
                .map(|change| (&change.app, &change.after.revision)),
            &revision,
            &format!(
                "after proposal {id} applied; undoing would overwrite that, so use \
                 `relish rollback` or an apply, which show their own diff"
            ),
        )?;
        if let Some(lease) = proposal
            .apps()
            .find_map(|app| leases.blocker(app, &by.principal_id, now_unix_ms))
        {
            return Err(ProposalRefusal::LeaseHeld {
                lease: Box::new(lease.clone()),
            });
        }
        Ok(proposal)
    }

    /// Record that applied proposal `id` was put back.
    pub fn mark_undone(
        &mut self,
        id: ProposalId,
        undone_by: Approval,
        now_unix_ms: u64,
        undo_proposal: Option<ProposalId>,
    ) -> Option<Proposal> {
        let proposal = self.proposals.get_mut(&id)?;
        let ProposalState::Applied {
            approval,
            at_unix_ms,
            ..
        } = &proposal.state
        else {
            return None;
        };
        proposal.state = ProposalState::Undone {
            approval: approval.clone(),
            applied_at_unix_ms: *at_unix_ms,
            undone_by,
            at_unix_ms: now_unix_ms,
            undo_proposal,
        };
        Some(proposal.clone())
    }

    /// Applied proposal `id`, if it can still be undone at `now_unix_ms`:
    /// reversible, not compacted, applied within [`UNDO_WINDOW_MS`] and
    /// among the newest [`UNDO_WINDOW_COUNT`] applied.
    pub fn undoable(&self, id: ProposalId, now_unix_ms: u64) -> Result<&Proposal, ProposalRefusal> {
        let proposal = self
            .proposals
            .get(&id)
            .ok_or(ProposalRefusal::NotFound { id })?;
        let ProposalState::Applied { .. } = proposal.state else {
            return Err(ProposalRefusal::WrongState {
                id,
                state: proposal.state.name().to_string(),
            });
        };
        if proposal.reversibility == Reversibility::Irreversible {
            return Err(ProposalRefusal::NoUndo {
                reason: format!("proposal {id} can't be undone: {}", proposal.undo.effect),
            });
        }
        if proposal.compacted || !self.in_undo_window(proposal, now_unix_ms) {
            let apps = proposal
                .apps()
                .map(|app| format!("relish rollback {} --namespace {}", app.name, app.namespace))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ProposalRefusal::NoUndo {
                reason: format!(
                    "proposal {id} is past its undo window (24 hours, newest 64); use {apps}"
                ),
            });
        }
        Ok(proposal)
    }

    fn in_undo_window(&self, proposal: &Proposal, now_unix_ms: u64) -> bool {
        let ProposalState::Applied { at_unix_ms, .. } = proposal.state else {
            return false;
        };
        if at_unix_ms.saturating_add(UNDO_WINDOW_MS) <= now_unix_ms {
            return false;
        }
        let newer = self
            .proposals
            .range(proposal.id..)
            .filter(|(_, other)| matches!(other.state, ProposalState::Applied { .. }))
            .count();
        newer <= UNDO_WINDOW_COUNT
    }

    /// Expire every pending proposal whose TTL has passed by
    /// `now_unix_ms`, ending its leases.
    pub fn expire(&mut self, leases: &mut AgentLeases, now_unix_ms: u64) -> Vec<Proposal> {
        let lapsed: Vec<ProposalId> = self
            .pending()
            .filter(|proposal| proposal.expires_at_unix_ms <= now_unix_ms)
            .map(|proposal| proposal.id)
            .collect();
        lapsed
            .into_iter()
            .filter_map(|id| {
                self.finish(
                    leases,
                    id,
                    ProposalState::Expired {
                        at_unix_ms: now_unix_ms,
                    },
                )
            })
            .collect()
    }

    /// Whether the reaper has anything to do at `now_unix_ms`.
    pub fn needs_reaping(&self, now_unix_ms: u64) -> bool {
        self.pending()
            .any(|proposal| proposal.expires_at_unix_ms <= now_unix_ms)
            || self.proposals.values().any(|proposal| {
                !proposal.state.is_pending()
                    && !proposal.compacted
                    && !self.in_undo_window(proposal, now_unix_ms)
            })
    }

    /// Drop the specs of every terminal proposal that can't be undone any
    /// more, and remove the oldest records beyond [`MAX_TERMINAL_KEPT`].
    pub fn compact(&mut self, now_unix_ms: u64) -> (Vec<ProposalId>, Vec<ProposalId>) {
        let to_compact: Vec<ProposalId> = self
            .proposals
            .values()
            .filter(|proposal| {
                !proposal.state.is_pending()
                    && !proposal.compacted
                    && !self.in_undo_window(proposal, now_unix_ms)
            })
            .map(|proposal| proposal.id)
            .collect();
        for id in &to_compact {
            if let Some(proposal) = self.proposals.get_mut(id) {
                for change in &mut proposal.changes {
                    change.before.spec = None;
                    change.after.spec = None;
                }
                proposal.compacted = true;
            }
        }
        let compacted: Vec<ProposalId> = self
            .proposals
            .values()
            .filter(|proposal| proposal.compacted)
            .map(|proposal| proposal.id)
            .collect();
        let excess = compacted.len().saturating_sub(MAX_TERMINAL_KEPT);
        let removed: Vec<ProposalId> = compacted.into_iter().take(excess).collect();
        for id in &removed {
            self.proposals.remove(id);
        }
        (to_compact, removed)
    }

    fn pending_proposal(
        &self,
        id: ProposalId,
        now_unix_ms: u64,
    ) -> Result<&Proposal, ProposalRefusal> {
        let proposal = self
            .proposals
            .get(&id)
            .ok_or(ProposalRefusal::NotFound { id })?;
        if !proposal.state.is_pending() {
            return Err(ProposalRefusal::WrongState {
                id,
                state: proposal.state.name().to_string(),
            });
        }
        // Past its TTL but not yet reaped: as good as expired.
        if proposal.expires_at_unix_ms <= now_unix_ms {
            return Err(ProposalRefusal::WrongState {
                id,
                state: "expired".to_string(),
            });
        }
        Ok(proposal)
    }

    fn finish(
        &mut self,
        leases: &mut AgentLeases,
        id: ProposalId,
        state: ProposalState,
    ) -> Option<Proposal> {
        let proposal = self.proposals.get_mut(&id)?;
        proposal.state = state;
        leases.end_pinned(id.0);
        Some(proposal.clone())
    }
}

/// A person deciding a proposal: a human token, and not its author.
fn check_decider(proposal: &Proposal, by: &Principal, action: &str) -> Result<(), ProposalRefusal> {
    if by.kind == TokenKind::Agent {
        return Err(ProposalRefusal::AgentDecides {
            action: action.to_string(),
        });
    }
    if proposal.author.may_be(by) {
        return Err(ProposalRefusal::SelfApproval {
            id: proposal.id,
            name: by.name.clone(),
        });
    }
    Ok(())
}

/// Every app still at the revision given beside it, or `Stale` naming the
/// first that isn't.
fn check_revisions<'a>(
    expected: impl Iterator<Item = (&'a AppId, &'a String)>,
    revision: &impl Fn(&AppId) -> String,
    what: &str,
) -> Result<(), ProposalRefusal> {
    for (app, expected) in expected {
        if revision(app) != *expected {
            return Err(ProposalRefusal::Stale {
                app: app.to_string(),
                what: what.to_string(),
            });
        }
    }
    Ok(())
}

/// A lease refusal as a proposal refusal: someone else's lease is
/// `lease_held`.
fn lease_refusal(refusal: LeaseRefusal) -> ProposalRefusal {
    match refusal {
        LeaseRefusal::Held { lease } => ProposalRefusal::LeaseHeld { lease },
        LeaseRefusal::HolderLimit { .. } | LeaseRefusal::ClusterLimit { .. } => {
            ProposalRefusal::Limit {
                reason: refusal.to_string(),
            }
        }
        other => ProposalRefusal::Invalid {
            reason: other.to_string(),
        },
    }
}

#[cfg(test)]
#[path = "machine_tests.rs"]
mod tests;
