//! What a proposal is: the change it makes, as each touched app before and
//! after, the words and numbers a human reads before approving it, and the
//! states it moves through.
//!
//! Everything here is plain data that lives in the council's
//! `DesiredState`, so it replicates, survives a leader change and goes into
//! snapshots like every other record. The rules that move a proposal from
//! state to state are in [`super::machine`]; building one is
//! [`super::build`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::app::AppSpec;
use crate::meat::AppId;
use crate::proposal::lease::AgentLease;
use crate::sesame::types::{ApiRole, TokenKind};

/// How long a proposal waits for approval when its author names no TTL.
pub const DEFAULT_PROPOSAL_TTL_MS: u64 = 60 * 60 * 1000;

/// The shortest TTL a proposal may ask for: 5 minutes.
pub const MIN_PROPOSAL_TTL_MS: u64 = 5 * 60 * 1000;

/// The longest TTL a proposal may ask for: 24 hours.
pub const MAX_PROPOSAL_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// The most pending proposals in the cluster.
pub const MAX_PENDING: usize = 64;

/// The most pending proposals one author may have.
pub const MAX_PENDING_PER_AUTHOR: usize = 8;

/// The most apps one proposal may touch. They're all in one namespace.
pub const MAX_APPS_PER_PROPOSAL: usize = 8;

/// The largest serialised proposal, in bytes.
pub const MAX_PROPOSAL_BYTES: usize = 64 * 1024;

/// The longest `reason`, in bytes.
pub const MAX_REASON_BYTES: usize = 1024;

/// How long after it applied a proposal can be undone.
pub const UNDO_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;

/// How many of the newest applied proposals can be undone.
pub const UNDO_WINDOW_COUNT: usize = 64;

/// How many terminal proposals the council keeps, compacted.
pub const MAX_TERMINAL_KEPT: usize = 256;

/// A proposal's number, from a council counter. Never reused on one
/// cluster.
// For the Rust reader: a newtype. `ProposalId(17)` and a plain `17u64`
// are different types, so a function that takes a `ProposalId` can't be
// handed a replica count by mistake. `#[serde(transparent)]` writes it as
// the bare number.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct ProposalId(pub u64);

impl std::fmt::Display for ProposalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Who did something to a proposal: the token's name, its kind, its role
/// and the exact credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    /// The token's name. "Never the author" compares this, so a rotated
    /// token, or one deleted and made again under the same name, is still
    /// its author.
    pub name: String,
    /// The exact credential (`AuthContext::principal_id`), which leases
    /// are held under.
    pub principal_id: String,
    /// Whether the token is a person's or an agent's.
    pub kind: TokenKind,
    /// The role the token was granted.
    pub role: ApiRole,
}

/// What kind of change a proposal makes. Every kind becomes "these apps go
/// from this state to that one"; the kind is kept for display, for
/// policies, and for the undo wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProposalKind {
    /// Apply `[app.*]` tables from a file, in one namespace.
    Apply,
    /// Change one app's replica count.
    Scale,
    /// Put one app back to its newest released spec that differs.
    Rollback,
    /// Stop one app.
    Stop,
    /// Delete one app.
    Delete,
    /// Undo an applied proposal: its `after` back to its `before`.
    Undo {
        /// The proposal being undone.
        of: ProposalId,
    },
}

impl ProposalKind {
    /// The kind's name, as policies, the CLI and audit records spell it.
    pub fn name(&self) -> &'static str {
        match self {
            ProposalKind::Apply => "apply",
            ProposalKind::Scale => "scale",
            ProposalKind::Rollback => "rollback",
            ProposalKind::Stop => "stop",
            ProposalKind::Delete => "delete",
            ProposalKind::Undo { .. } => "undo",
        }
    }
}

/// How completely the undo puts things back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reversibility {
    /// Every touched app goes back to exactly its `before` state.
    Exact,
    /// The specs come back, but something else doesn't: data on a deleted
    /// app's managed volumes.
    Lossy,
    /// It can't be undone.
    #[serde(rename = "none")]
    Irreversible,
}

impl Reversibility {
    /// The word the JSON and policies use.
    pub fn as_str(self) -> &'static str {
        match self {
            Reversibility::Exact => "exact",
            Reversibility::Lossy => "lossy",
            Reversibility::Irreversible => "none",
        }
    }
}

/// One app as it was, or as it will be.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppRevision {
    /// The stored spec. `None` means the app doesn't exist, or, once the
    /// proposal is compacted, that the spec has been dropped.
    pub spec: Option<Box<AppSpec>>,
    /// Whether the app is stopped.
    pub stopped: bool,
    /// [`super::revision::revision_of`] the spec and the stopped flag, in
    /// hex.
    pub revision: String,
}

impl AppRevision {
    /// The revision of `spec` with `stopped`.
    pub fn new(spec: Option<AppSpec>, stopped: bool) -> Self {
        let revision = super::revision::revision_of(spec.as_ref(), stopped);
        AppRevision {
            spec: spec.map(Box::new),
            stopped,
            revision,
        }
    }

    /// Whether the app exists in this revision.
    pub fn exists(&self) -> bool {
        self.revision != super::revision::absent_revision()
    }

    /// The replica count, when the spec has a fixed one.
    pub fn replicas(&self) -> Option<u32> {
        match self.spec.as_deref()?.replicas {
            crate::config::Replicas::Fixed(count) => Some(count),
            crate::config::Replicas::DaemonSet => None,
        }
    }
}

/// One touched app: before and after.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppChange {
    /// The app.
    pub app: AppId,
    /// As it was when the proposal was made.
    pub before: AppRevision,
    /// As it will be once applied.
    pub after: AppRevision,
}

/// One changed field, in words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldChange {
    /// The app, `namespace/name`.
    pub app: String,
    /// The dotted field path, e.g. `replicas` or `env.LOG_LEVEL`.
    pub field: String,
    /// The old value, redacted; `(secret)` for a secret.
    pub from: Option<String>,
    /// The new value, redacted; `(secret)` for a secret.
    pub to: Option<String>,
}

/// What a proposal touches, computed by the leader from its own state,
/// never claimed by the author.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlastRadius {
    /// Every touched app, `namespace/name`.
    pub apps: Vec<String>,
    /// The nodes those apps are placed on now.
    pub nodes: Vec<String>,
    /// Where the scheduler would put new instances. Empty in this version:
    /// the scheduler places them when the proposal applies.
    pub new_nodes_forecast: Vec<String>,
    /// Instances a new spec replaces.
    pub instances_replaced: u32,
    /// Instances it starts.
    pub instances_started: u32,
    /// Instances it stops.
    pub instances_stopped: u32,
    /// Ingress hosts and paths served by these apps.
    pub routes: Vec<String>,
    /// Service names other apps resolve these apps by.
    pub services: Vec<String>,
    /// What the leader couldn't see, and why. Never silent.
    pub unknown: Vec<String>,
}

impl BlastRadius {
    /// Every instance it touches: replaced, started and stopped.
    pub fn instances(&self) -> u32 {
        self.instances_replaced
            .saturating_add(self.instances_started)
            .saturating_add(self.instances_stopped)
    }
}

/// How to undo a proposal, shown before it's approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoPlan {
    /// The command, `relish undo 17`; `None` when it can't be undone.
    pub command: Option<String>,
    /// What undoing does, or why it can't.
    pub effect: String,
}

/// Who approved a proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum Approval {
    /// A person, with a human token.
    Human(Principal),
    /// An `approve` policy an admin wrote.
    Policy {
        /// The policy's name.
        name: String,
    },
}

impl Approval {
    /// How audit records and listings name the approver: the token's name,
    /// or `policy:<name>`.
    pub fn label(&self) -> String {
        match self {
            Approval::Human(principal) => principal.name.clone(),
            Approval::Policy { name } => format!("policy:{name}"),
        }
    }
}

/// Why an approved proposal wrote nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum FailedReason {
    /// An app changed after the proposal was made.
    Stale {
        /// The app that moved.
        app: AppId,
        /// The revision the proposal saw.
        expected: String,
        /// The revision it has now.
        found: String,
    },
    /// Someone other than the author holds a lease on a touched app.
    Lease {
        /// The lease in the way.
        lease: Box<AgentLease>,
    },
    /// A guardrail or app policy refused it at approval time.
    Guardrail {
        /// The built-in rule or policy that refused.
        rule: String,
        /// Its message.
        message: String,
    },
    /// One of the app writes was refused, so none were made.
    Refused {
        /// What refused it.
        reason: String,
    },
}

impl std::fmt::Display for FailedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FailedReason::Stale { app, .. } => {
                write!(f, "{app} changed after the proposal was made")
            }
            FailedReason::Lease { lease } => write!(
                f,
                "{} is held by {} ({})",
                lease.target, lease.holder.name, lease.purpose
            ),
            FailedReason::Guardrail { rule, message } => write!(f, "{rule}: {message}"),
            FailedReason::Refused { reason } => write!(f, "{reason}"),
        }
    }
}

/// Where a proposal is in its life. Every state but `Pending` is final.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProposalState {
    /// Waiting for a human or a policy.
    Pending,
    /// Approved and written, in one Raft entry.
    Applied {
        /// Who approved it.
        approval: Approval,
        /// When, on the leader's clock.
        at_unix_ms: u64,
        /// The Raft log index of the entry that applied it.
        log_index: u64,
    },
    /// A person said no.
    Rejected {
        /// Who.
        by: Principal,
        /// Why, in their words.
        reason: String,
        /// When.
        at_unix_ms: u64,
    },
    /// Nobody decided before its TTL ran out.
    Expired {
        /// When the reaper expired it.
        at_unix_ms: u64,
    },
    /// Its author took it back.
    Withdrawn {
        /// When.
        at_unix_ms: u64,
    },
    /// Approved, but nothing was written.
    Failed {
        /// Who approved it.
        approval: Approval,
        /// Why nothing was written.
        why: FailedReason,
        /// When.
        at_unix_ms: u64,
    },
    /// Applied, then put back.
    Undone {
        /// Who approved it in the first place.
        approval: Approval,
        /// When it applied.
        applied_at_unix_ms: u64,
        /// Who undid it: a person directly, or whoever approved an agent's
        /// undo proposal.
        undone_by: Approval,
        /// When.
        at_unix_ms: u64,
        /// The undo proposal, when an agent proposed the undo.
        undo_proposal: Option<ProposalId>,
    },
}

impl ProposalState {
    /// The state's name: `pending`, `applied` and so on.
    pub fn name(&self) -> &'static str {
        match self {
            ProposalState::Pending => "pending",
            ProposalState::Applied { .. } => "applied",
            ProposalState::Rejected { .. } => "rejected",
            ProposalState::Expired { .. } => "expired",
            ProposalState::Withdrawn { .. } => "withdrawn",
            ProposalState::Failed { .. } => "failed",
            ProposalState::Undone { .. } => "undone",
        }
    }

    /// Whether it's still waiting.
    pub fn is_pending(&self) -> bool {
        matches!(self, ProposalState::Pending)
    }
}

/// A change an agent (or a person) asked for, waiting for, or past, its
/// approval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    /// Its number. The state machine assigns it; a request's is ignored.
    pub id: ProposalId,
    /// What kind of change.
    pub kind: ProposalKind,
    /// The one namespace every touched app is in.
    pub namespace: String,
    /// Who proposed it.
    pub author: Principal,
    /// The author's stated reason. Untrusted: redacted, no control
    /// characters, at most [`MAX_REASON_BYTES`].
    pub reason: String,
    /// The author's key for a retried request: the same author and key
    /// return the proposal already made.
    pub idempotency_key: Option<String>,
    /// When it was made, on the leader's clock.
    pub created_at_unix_ms: u64,
    /// When it expires if nobody decides.
    pub expires_at_unix_ms: u64,
    /// Where it is.
    pub state: ProposalState,
    /// Each touched app before and after, in app order.
    pub changes: Vec<AppChange>,
    /// The plain-language summary, written by the cluster.
    pub summary: Vec<String>,
    /// The field-by-field diff.
    pub diff: Vec<FieldChange>,
    /// What it touches.
    pub blast_radius: BlastRadius,
    /// How completely it can be undone.
    pub reversibility: Reversibility,
    /// The undo command and what it does.
    pub undo: UndoPlan,
    /// What `warn` and `audit` policies said when it was made.
    pub findings: Vec<crate::policy::Finding>,
    /// True once the reaper has dropped the specs: the record stays, the
    /// undo doesn't.
    #[serde(default)]
    pub compacted: bool,
}

impl Proposal {
    /// The touched apps.
    pub fn apps(&self) -> impl Iterator<Item = &AppId> {
        self.changes.iter().map(|change| &change.app)
    }

    /// The facts its audit records carry.
    pub fn audit_details(&self) -> BTreeMap<String, String> {
        let mut details = BTreeMap::from([
            ("proposal".to_string(), self.id.to_string()),
            ("kind".to_string(), self.kind.name().to_string()),
            ("author".to_string(), self.author.name.clone()),
            (
                "author_kind".to_string(),
                self.author.kind.as_str().to_string(),
            ),
            ("reason".to_string(), self.reason.clone()),
            (
                "reversibility".to_string(),
                self.reversibility.as_str().to_string(),
            ),
            ("apps".to_string(), self.blast_radius.apps.join(",")),
            ("nodes".to_string(), self.blast_radius.nodes.join(",")),
            ("state".to_string(), self.state.name().to_string()),
        ]);
        if let Some(command) = &self.undo.command {
            details.insert("undo".to_string(), command.clone());
        }
        if !self.findings.is_empty() {
            details.insert(
                "findings".to_string(),
                self.findings
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
            );
        }
        match &self.state {
            ProposalState::Applied { approval, .. } | ProposalState::Undone { approval, .. } => {
                details.insert("approved_by".to_string(), approval.label());
            }
            ProposalState::Failed { approval, why, .. } => {
                details.insert("approved_by".to_string(), approval.label());
                details.insert("why".to_string(), why.to_string());
            }
            _ => {}
        }
        if let ProposalKind::Undo { of } = self.kind {
            details.insert("undo_of".to_string(), of.to_string());
        }
        details
    }
}

/// Why a proposal entry changed nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "refusal", rename_all = "snake_case")]
pub enum ProposalRefusal {
    /// There's no such proposal.
    #[error("there's no proposal {id}")]
    NotFound { id: ProposalId },
    /// It isn't pending, or for undo, isn't applied.
    #[error("proposal {id} is {state}")]
    WrongState { id: ProposalId, state: String },
    /// Only the author may withdraw it.
    #[error("only {author}, who made proposal {id}, may withdraw it")]
    NotAuthor { id: ProposalId, author: String },
    /// Nobody approves, rejects or undoes their own proposal.
    #[error("{name} made proposal {id} and can't approve it; another person must")]
    SelfApproval { id: ProposalId, name: String },
    /// An agent token approves, rejects and undoes nothing.
    #[error("an agent token can't {action} proposals; a person with a human token does")]
    AgentDecides { action: String },
    /// Over one of the limits.
    #[error("{reason}")]
    Limit { reason: String },
    /// Another pending proposal already touches the app.
    #[error("proposal {proposal} is already pending on {app}; withdraw it, or wait for it")]
    Conflict { app: String, proposal: ProposalId },
    /// Someone else holds a lease on a touched app.
    #[error("{} is held by {} ({})", lease.target, lease.holder.name, lease.purpose)]
    LeaseHeld { lease: Box<AgentLease> },
    /// An app moved since the proposal (or its undo) saw it.
    #[error("{app} changed {what}")]
    Stale { app: String, what: String },
    /// A guardrail, app policy or built-in rule refused it.
    #[error("{rule}: {message}")]
    Guardrail { rule: String, message: String },
    /// It can't be undone: irreversible, compacted, or out of the window.
    #[error("{reason}")]
    NoUndo { reason: String },
    /// The request itself is wrong.
    #[error("{reason}")]
    Invalid { reason: String },
}

impl ProposalRefusal {
    /// The `code` an API answer and the agent envelope carry.
    pub fn code(&self) -> &'static str {
        match self {
            ProposalRefusal::NotFound { .. } => "not_found",
            ProposalRefusal::WrongState { .. } | ProposalRefusal::Conflict { .. } => "conflict",
            ProposalRefusal::NotAuthor { .. }
            | ProposalRefusal::SelfApproval { .. }
            | ProposalRefusal::AgentDecides { .. } => "forbidden",
            ProposalRefusal::Limit { .. } => "proposal_limit",
            ProposalRefusal::LeaseHeld { .. } => "lease_held",
            ProposalRefusal::Stale { .. } => "stale",
            ProposalRefusal::Guardrail { .. } => "guardrail",
            ProposalRefusal::NoUndo { .. } => "no_undo",
            ProposalRefusal::Invalid { .. } => "invalid",
        }
    }
}

/// What a proposal Raft entry did. Carried back in
/// `CouncilResponse::Proposal`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ProposalOutcome {
    /// A new proposal, pending.
    Created { proposal: Box<Proposal> },
    /// The author retried with the same idempotency key: the proposal it
    /// already made.
    Existing { proposal: Box<Proposal> },
    /// Approved and applied.
    Applied { proposal: Box<Proposal> },
    /// Approved; nothing written.
    Failed { proposal: Box<Proposal> },
    /// Rejected by a person.
    Rejected { proposal: Box<Proposal> },
    /// Withdrawn by its author.
    Withdrawn { proposal: Box<Proposal> },
    /// Put back by a person's direct undo.
    Undone { proposal: Box<Proposal> },
    /// The reaper expired these.
    Expired { proposals: Vec<Proposal> },
    /// The reaper compacted these and removed the oldest beyond the limit.
    Compacted {
        compacted: Vec<ProposalId>,
        removed: Vec<ProposalId>,
    },
    /// Nothing changed, and why.
    Refused { refusal: ProposalRefusal },
}
