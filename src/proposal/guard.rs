//! Where policies judge a proposal: guardrails and policy auto-approval
//! (#1682).
//!
//! The leader calls [`judge_proposal`] twice in a proposal's life: before
//! it writes `ProposalCreate` ([`GuardStage::Create`], the caller is the
//! author) and before it writes `ProposalApply` ([`GuardStage::Approve`],
//! the caller is the person who approves, or the author for a policy
//! approval, since nobody else stands behind one). It reads the policies
//! and their generation together, judges on a blocking thread, and the
//! Raft entry carries that generation, so a policy written in between
//! makes the leader judge again.
//! The state machine never runs a policy.
//!
//! A judgement runs, in order:
//!
//! 1. the app policies on every `after` spec that changes, exactly as
//!    `relish apply` judges them, with `source = "proposal"`;
//! 2. the guardrails, policies with `applies_to = "proposals"`, on the
//!    proposal's [`PolicyInput`]: what it changes and who asked;
//! 3. the `approve` guardrails, the first of which (by name) approves it,
//!    unless [`held_for_a_person`] (built-in rule 3) says a person must.
//!
//! The first `deny` refuses; `warn` and `audit` findings are stored on the
//! proposal and shown first to the approver. Then:
//!
//! - a [`GuardVerdict::refusal`] at `Create` refuses the proposal with the
//!   error code `guardrail`; at `Approve` it records `Failed`;
//! - a [`GuardVerdict::approve`] at `Create` applies the proposal at once
//!   with `Approval::Policy`; the leader's reaper also judges every pending
//!   proposal at `Approve` on each pass while [`may_auto_approve`] is true,
//!   so a new leader finishes what the old one didn't, and a policy written
//!   after a proposal still approves it.

use std::collections::BTreeMap;

use super::types::{Principal, Proposal, ProposalKind, Reversibility};
use crate::config::app::AppSpec;
use crate::config::{PolicyAction, PolicySpec, PolicyTarget};
use crate::meat::AppId;
use crate::policy::{
    AuthorFacts, Caller, Finding, PolicyInput, PolicySet, ProposalFacts, Source,
    judge_proposal_input,
};

/// When a proposal is judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardStage {
    /// Before it's stored; the caller is its author.
    Create,
    /// Before it's applied; the caller is the person who approves, or the
    /// author for a policy approval.
    Approve,
}

/// Everything a judgement may read. There's no clock and no `reason`:
/// policies are pure functions of their arguments, and the reason is the
/// author's free text, which a guardrail mustn't be talked round by.
#[derive(Debug, Clone, Copy)]
pub struct GuardInput<'a> {
    /// The proposal, as built by the leader.
    pub proposal: &'a Proposal,
    /// Who's asking now.
    pub caller: &'a Principal,
    /// Which judgement this is.
    pub stage: GuardStage,
    /// The policies in force, read with their generation.
    pub policies: &'a BTreeMap<String, PolicySpec>,
}

/// A rule that refused a proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardRefusal {
    /// A built-in rule's name, or `policy:<name>`.
    pub rule: String,
    /// What it said.
    pub message: String,
}

/// What the policies made of a proposal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuardVerdict {
    /// Set when a rule refuses it.
    pub refusal: Option<GuardRefusal>,
    /// `warn` and `audit` findings, stored on the proposal and shown first
    /// to the approver.
    pub findings: Vec<Finding>,
    /// The name of the `approve` policy that approves it, if one does.
    pub approve: Option<String>,
    /// When an `approve` policy matched but built-in rule 3 kept the
    /// proposal for a person: the policy and why, in words.
    pub held: Option<String>,
}

/// Judge `input.proposal`. See the module documentation.
pub fn judge_proposal(input: &GuardInput<'_>) -> GuardVerdict {
    let mut verdict = GuardVerdict::default();
    let Some(set) = compile(input.policies, &mut verdict) else {
        return verdict;
    };
    let caller = Caller::token(&input.caller.name, input.caller.role);
    let changed = input.proposal.changes.iter().filter_map(|change| {
        let after = change.after.spec.as_deref()?;
        (change.before.spec.as_deref() != Some(after)).then_some((&change.app, after))
    });
    judge_specs_with(&set, &caller, changed, &mut verdict);

    let facts = policy_input(input.proposal);
    let judged = judge_proposal_input(&set, &facts, &caller);
    if verdict.refusal.is_none()
        && let Some(denial) = judged.verdict.denials.first()
    {
        verdict.refusal = Some(GuardRefusal {
            rule: format!("policy:{}", denial.policy),
            message: denial.message.clone(),
        });
    }
    verdict.findings.extend(judged.verdict.warnings);
    verdict.findings.extend(judged.verdict.audits);

    let Some(name) = judged.approve else {
        return verdict;
    };
    if verdict.refusal.is_some() {
        return verdict;
    }
    let warned = verdict
        .findings
        .iter()
        .chain(&input.proposal.findings)
        .any(|finding| finding.action == PolicyAction::Warn);
    match held_for_a_person(
        input.proposal.reversibility,
        &input.proposal.blast_radius.unknown,
        warned,
    ) {
        Some(why) => verdict.held = Some(format!("policy {name} matched, but {why}")),
        None => verdict.approve = Some(name),
    }
    verdict
}

/// Built-in rule 3, which no policy can lift: why a person must approve
/// this proposal even though an `approve` policy matched it, or `None`
/// when a policy may. A policy never approves a change that can't be put
/// back exactly, one a `warn` policy flagged, or one whose blast radius
/// the leader couldn't fully see.
pub fn held_for_a_person(
    reversibility: Reversibility,
    unknown: &[String],
    warned: bool,
) -> Option<&'static str> {
    // `restart` will be the exception to the reversibility rule when it
    // arrives (#1682): it changes no spec, so there's nothing to lose.
    if reversibility != Reversibility::Exact {
        return Some("it can't be undone exactly, so a person must approve it");
    }
    if warned {
        return Some("a warn policy flagged it, so a person must approve it");
    }
    if !unknown.is_empty() {
        return Some(
            "the leader couldn't see all of its blast radius, so a person must approve it",
        );
    }
    None
}

/// Whether any policy could approve a proposal, so the reaper should judge
/// the pending ones: an `approve` policy that applies to proposals.
pub fn may_auto_approve(policies: &BTreeMap<String, PolicySpec>) -> bool {
    policies.values().any(|policy| {
        policy.action == PolicyAction::Approve && policy.applies_to == PolicyTarget::Proposals
    })
}

/// What a guardrail sees of `proposal`: the change, as the leader computed
/// it, and who asked. Never the `reason`.
pub fn policy_input(proposal: &Proposal) -> PolicyInput {
    let mut fields: Vec<String> = proposal
        .diff
        .iter()
        .map(|change| change.field.clone())
        .collect();
    fields.sort();
    fields.dedup();
    let mut images: Vec<String> = proposal
        .changes
        .iter()
        .filter_map(|change| change.after.spec.as_deref()?.image.clone())
        .collect();
    images.sort();
    images.dedup();
    let (replicas_from, replicas_to) = match proposal.kind {
        ProposalKind::Scale => proposal
            .changes
            .first()
            .map(|change| {
                (
                    change.before.replicas().unwrap_or(0),
                    change.after.replicas().unwrap_or(0),
                )
            })
            .unwrap_or_default(),
        _ => (0, 0),
    };
    PolicyInput {
        proposal: ProposalFacts {
            kind: proposal.kind.name().to_string(),
            namespace: proposal.namespace.clone(),
            apps: proposal.apps().map(|app| app.name.clone()).collect(),
            nodes: proposal.blast_radius.nodes.clone(),
            instances: proposal.blast_radius.instances(),
            replicas_from,
            replicas_to,
            reversibility: proposal.reversibility.as_str().to_string(),
            fields,
            images,
        },
        author: AuthorFacts {
            name: proposal.author.name.clone(),
            kind: proposal.author.kind.as_str().to_string(),
            role: proposal.author.role.to_string(),
        },
    }
}

/// Judge app specs by the app policies, as `relish apply` would with
/// `source = "proposal"`: the first denial refuses, warnings and audit
/// findings are kept. A policy that no longer compiles refuses, failing
/// closed. A direct `relish undo` judges the `before` specs it puts back
/// through this too.
pub fn judge_specs<'a>(
    policies: &BTreeMap<String, PolicySpec>,
    caller: &Principal,
    specs: impl Iterator<Item = (&'a AppId, &'a AppSpec)>,
) -> GuardVerdict {
    let mut verdict = GuardVerdict::default();
    if let Some(set) = compile(policies, &mut verdict) {
        let caller = Caller::token(&caller.name, caller.role);
        judge_specs_with(&set, &caller, specs, &mut verdict);
    }
    verdict
}

/// `policies` compiled, or `None`: when there are none, or, with a refusal
/// in `verdict`, when one no longer compiles.
fn compile(
    policies: &BTreeMap<String, PolicySpec>,
    verdict: &mut GuardVerdict,
) -> Option<PolicySet> {
    if policies.is_empty() {
        return None;
    }
    match PolicySet::compile(policies) {
        Ok(set) => Some(set),
        Err(error) => {
            verdict.refusal = Some(GuardRefusal {
                rule: "policy".to_string(),
                message: error.to_string(),
            });
            None
        }
    }
}

fn judge_specs_with<'a>(
    set: &PolicySet,
    caller: &Caller,
    specs: impl Iterator<Item = (&'a AppId, &'a AppSpec)>,
    verdict: &mut GuardVerdict,
) {
    for (app, spec) in specs {
        let judged = crate::policy::judge_app(set, app, spec, caller, Source::Proposal);
        if verdict.refusal.is_none()
            && let Some(denial) = judged.verdict.denials.first()
        {
            verdict.refusal = Some(GuardRefusal {
                rule: format!("policy:{}", denial.policy),
                message: denial.message.clone(),
            });
        }
        verdict.findings.extend(judged.verdict.warnings);
        verdict.findings.extend(judged.verdict.audits);
    }
}

/// `spec` with the policy defaults filled in, as `relish apply` stores it.
/// A policy set that doesn't compile fills nothing; [`judge_specs`] then
/// refuses the proposal.
pub fn fill_defaults(
    policies: &BTreeMap<String, PolicySpec>,
    caller: &Principal,
    app: &AppId,
    spec: &AppSpec,
) -> AppSpec {
    if policies.is_empty() {
        return spec.clone();
    }
    let Ok(set) = PolicySet::compile(policies) else {
        return spec.clone();
    };
    let caller = Caller::token(&caller.name, caller.role);
    crate::policy::judge_app(&set, app, spec, &caller, Source::Proposal).spec
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
