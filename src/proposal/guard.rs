//! Where policies judge a proposal: the seam for guardrails and policy
//! auto-approval (#1682).
//!
//! The leader calls [`judge_proposal`] twice in a proposal's life: before
//! it writes `ProposalCreate` ([`GuardStage::Create`], the caller is the
//! author) and before it writes `ProposalApply` ([`GuardStage::Approve`],
//! the caller is the approver). It reads the policies and their generation
//! together, judges on a blocking thread, and the Raft entry carries that
//! generation, so a policy written in between makes the leader judge again.
//! The state machine never runs a policy.
//!
//! This version runs the app policies on every `after` spec that changes,
//! exactly as `relish apply` judges them, with `source = "proposal"`: a
//! deny refuses, a warn or an audit becomes a finding stored on the
//! proposal. It never approves. Guardrail policies (`applies_to =
//! "proposals"`) and the `approve` action extend this function, and
//! [`may_auto_approve`], without changing any caller:
//!
//! - a [`GuardVerdict::refusal`] at `Create` refuses the proposal with the
//!   error code `guardrail`; at `Approve` it records `Failed`;
//! - a [`GuardVerdict::approve`] at `Create` applies the proposal at once
//!   with `Approval::Policy`; the leader's reaper also judges every pending
//!   proposal at `Approve` on each pass while [`may_auto_approve`] is true,
//!   so a new leader finishes what the old one didn't.

use std::collections::BTreeMap;

use super::types::{Principal, Proposal};
use crate::config::PolicySpec;
use crate::config::app::AppSpec;
use crate::meat::AppId;
use crate::policy::{Caller, Finding, PolicySet, Source};

/// When a proposal is judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardStage {
    /// Before it's stored; the caller is its author.
    Create,
    /// Before it's applied; the caller is the approver, or the cluster for
    /// a policy approval.
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
}

/// Judge `input.proposal`. See the module documentation.
pub fn judge_proposal(input: &GuardInput<'_>) -> GuardVerdict {
    let changed = input.proposal.changes.iter().filter_map(|change| {
        let after = change.after.spec.as_deref()?;
        (change.before.spec.as_deref() != Some(after)).then_some((&change.app, after))
    });
    judge_specs(input.policies, input.caller, changed)
}

/// Whether any policy could approve a proposal, so the reaper should judge
/// the pending ones. Always `false` until the `approve` action exists.
pub fn may_auto_approve(_policies: &BTreeMap<String, PolicySpec>) -> bool {
    false
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
    if policies.is_empty() {
        return verdict;
    }
    let set = match PolicySet::compile(policies) {
        Ok(set) => set,
        Err(error) => {
            verdict.refusal = Some(GuardRefusal {
                rule: "policy".to_string(),
                message: error.to_string(),
            });
            return verdict;
        }
    };
    let caller = Caller::token(&caller.name, caller.role);
    for (app, spec) in specs {
        let judged = crate::policy::judge_app(&set, app, spec, &caller, Source::Proposal);
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
    verdict
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
