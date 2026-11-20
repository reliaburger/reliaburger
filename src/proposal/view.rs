//! How a proposal looks to a reader: `GET /v1/proposals`, `relish proposal
//! show` and the `data` of `relish agent propose`.
//!
//! A view has no specs in it, only what a person needs to decide: the
//! cluster's summary, reversibility and undo, policy findings, the blast
//! radius, the field diff, and the author's reason, last.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::types::*;

/// One proposal, as readers see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProposalView {
    /// Its number.
    pub id: u64,
    /// `pending`, `applied`, `rejected`, `expired`, `withdrawn`, `failed` or
    /// `undone`.
    pub state: String,
    /// `apply`, `scale`, `rollback`, `stop`, `delete`, `restart`,
    /// `cancel_deploy`, `clear_fault` or `undo`.
    pub kind: String,
    /// For an `undo`, the proposal it undoes.
    pub undo_of: Option<u64>,
    /// The namespace every touched app is in.
    pub namespace: String,
    /// The author's token name.
    pub author: String,
    /// `human` or `agent`.
    pub author_kind: String,
    /// The cluster's plain-language summary, in order.
    pub summary: Vec<String>,
    /// `exact`, `lossy` or `none`.
    pub reversibility: String,
    /// How to undo it, and what that does.
    pub undo: UndoView,
    /// What `warn` and `audit` policies said, each as one line.
    pub findings: Vec<String>,
    /// What it touches.
    pub blast_radius: BlastRadiusView,
    /// Every changed field.
    pub diff: Vec<FieldChangeView>,
    /// Who decided, or who still must.
    pub approval: ApprovalView,
    /// The leases it holds while pending: `app:shop/web` targets.
    pub leases: Vec<String>,
    /// Each app's revision before and after, the first 12 hex digits.
    pub revisions: Vec<RevisionView>,
    /// When it was made, Unix milliseconds on the leader's clock.
    pub created_at_unix_ms: u64,
    /// When it expires if pending.
    pub expires_at_unix_ms: u64,
    /// Seconds until it expires, when pending; 0 otherwise.
    pub expires_in_seconds: u64,
    /// Why it failed or was rejected, when it did.
    pub outcome: Option<String>,
    /// True once the specs have been dropped: the record stays, the undo
    /// doesn't.
    pub compacted: bool,
    /// What a guardrail policy sees of it, `proposal` and `author`, for
    /// `relish policy test --proposal` (#1682). Never the reason.
    #[serde(default)]
    pub policy_input: crate::policy::PolicyInput,
    /// The author's stated reason, in its own words. Untrusted.
    pub reason: String,
}

/// How to undo a proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UndoView {
    /// `relish undo 17`, or absent when it can't be undone.
    pub command: Option<String>,
    /// What undoing does, or why it can't.
    pub effect: String,
}

/// Who decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalView {
    /// `human` while a person must still decide; `decided` once someone did.
    pub needs: String,
    /// The approver: a token name, or `policy:<name>`.
    pub approved_by: Option<String>,
    /// Who rejected it.
    pub rejected_by: Option<String>,
    /// Who undid it.
    pub undone_by: Option<String>,
}

/// The blast radius, as readers see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BlastRadiusView {
    /// Every touched app, `namespace/name`.
    pub apps: Vec<String>,
    /// The nodes they're on now.
    pub nodes: Vec<String>,
    /// Where the scheduler's dry run would put the instances it starts. A
    /// forecast: the scheduler decides when it applies.
    pub new_nodes_forecast: Vec<String>,
    /// How many of them the dry run couldn't place.
    #[serde(default)]
    pub unplaced_forecast: u32,
    /// Why it couldn't, or why there's no forecast.
    #[serde(default)]
    pub forecast_note: Option<String>,
    /// Instances replaced by a new spec.
    pub instances_replaced: u32,
    /// Instances started.
    pub instances_started: u32,
    /// Instances stopped.
    pub instances_stopped: u32,
    /// Ingress hosts and paths.
    pub routes: Vec<String>,
    /// Service names.
    pub services: Vec<String>,
    /// What the leader couldn't see.
    pub unknown: Vec<String>,
}

/// One changed field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FieldChangeView {
    /// `namespace/name`.
    pub app: String,
    /// The dotted field path.
    pub field: String,
    /// The old value, redacted.
    pub from: Option<String>,
    /// The new value, redacted.
    pub to: Option<String>,
}

/// One app's revisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RevisionView {
    /// `namespace/name`.
    pub app: String,
    /// Before, 12 hex digits.
    pub before: String,
    /// After, 12 hex digits.
    pub after: String,
}

/// What `GET /v1/proposals` answers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProposalListing {
    /// The proposals the caller may see, newest first.
    pub proposals: Vec<ProposalView>,
}

/// `POST /v1/proposals/{id}/reject`'s body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RejectRequest {
    /// Why, for the author and the audit log.
    #[serde(default)]
    pub reason: String,
}

/// The first 12 hex digits of `revision`, as people read it.
pub(super) fn short(revision: &str) -> String {
    revision.chars().take(12).collect()
}

impl Proposal {
    /// How it looks to a reader at `now_unix_ms`.
    pub fn view(&self, now_unix_ms: u64) -> ProposalView {
        let mut approval = ApprovalView {
            needs: if self.state.is_pending() {
                "human".to_string()
            } else {
                "decided".to_string()
            },
            approved_by: None,
            rejected_by: None,
            undone_by: None,
        };
        let mut outcome = None;
        match &self.state {
            ProposalState::Applied { approval: by, .. } => {
                approval.approved_by = Some(by.label());
            }
            ProposalState::Undone {
                approval: by,
                undone_by,
                ..
            } => {
                approval.approved_by = Some(by.label());
                approval.undone_by = Some(undone_by.label());
            }
            ProposalState::Failed {
                approval: by, why, ..
            } => {
                approval.approved_by = Some(by.label());
                outcome = Some(why.to_string());
            }
            ProposalState::Rejected { by, reason, .. } => {
                approval.rejected_by = Some(by.name.clone());
                outcome = Some(reason.clone());
            }
            ProposalState::Pending | ProposalState::Expired { .. } => {}
            ProposalState::Withdrawn { .. } => {
                outcome = Some("withdrawn by its author".to_string());
            }
        }
        let radius = &self.blast_radius;
        ProposalView {
            id: self.id.0,
            state: self.state.name().to_string(),
            kind: self.kind.name().to_string(),
            undo_of: match self.kind {
                ProposalKind::Undo { of } => Some(of.0),
                _ => None,
            },
            namespace: self.namespace.clone(),
            author: self.author.name.clone(),
            author_kind: self.author.kind.as_str().to_string(),
            summary: self.summary.clone(),
            reversibility: self.reversibility.as_str().to_string(),
            undo: UndoView {
                command: self.undo.command.clone(),
                effect: self.undo.effect.clone(),
            },
            findings: self.findings.iter().map(ToString::to_string).collect(),
            blast_radius: BlastRadiusView {
                apps: radius.apps.clone(),
                nodes: radius.nodes.clone(),
                new_nodes_forecast: radius.new_nodes_forecast.clone(),
                unplaced_forecast: radius.unplaced_forecast,
                forecast_note: radius.forecast_note.clone(),
                instances_replaced: radius.instances_replaced,
                instances_started: radius.instances_started,
                instances_stopped: radius.instances_stopped,
                routes: radius.routes.clone(),
                services: radius.services.clone(),
                unknown: radius.unknown.clone(),
            },
            diff: self
                .diff
                .iter()
                .map(|change| FieldChangeView {
                    app: change.app.clone(),
                    field: change.field.clone(),
                    from: change.from.clone(),
                    to: change.to.clone(),
                })
                .collect(),
            approval,
            leases: if self.state.is_pending() {
                self.apps()
                    .map(|app| super::lease::LeaseTarget::app(&app.namespace, &app.name).key())
                    .collect()
            } else {
                Vec::new()
            },
            revisions: self
                .changes
                .iter()
                .map(|change| RevisionView {
                    app: change.app.to_string(),
                    before: short(&change.before.revision),
                    after: short(&change.after.revision),
                })
                .collect(),
            created_at_unix_ms: self.created_at_unix_ms,
            expires_at_unix_ms: self.expires_at_unix_ms,
            expires_in_seconds: if self.state.is_pending() {
                self.expires_at_unix_ms.saturating_sub(now_unix_ms) / 1000
            } else {
                0
            },
            outcome,
            compacted: self.compacted,
            policy_input: super::guard::policy_input(self),
            reason: self.reason.clone(),
        }
    }
}
