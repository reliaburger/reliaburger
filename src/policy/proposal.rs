//! Guardrails: policies that judge a proposed change rather than a
//! workload (#1682).
//!
//! A policy with `applies_to = "proposals"` sees two variables of its own:
//! `proposal`, the change as the leader computed it ([`ProposalFacts`]),
//! and `author`, who asked for it ([`AuthorFacts`]), beside the `caller`
//! and `source` every policy sees. A `deny`, `warn` or `audit` guardrail
//! works as it does for an app. An `approve` guardrail has only a `match`,
//! and the first one in name order whose `match` is true approves the
//! proposal, subject to the built-in rules in `crate::proposal::guard`.
//!
//! Nothing here reads the proposal's `reason`. It's the author's free text,
//! and a guardrail that read it could be talked round by it.

use std::collections::HashMap;
use std::sync::Arc;

use cel::{Context, Value};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    Caller, Finding, PolicySet, Source, Verdict, WorkloadKind, decide, json_to_cel, matches,
};
use crate::config::{PolicyAction, PolicyTarget};
use crate::meat::AppId;

/// A proposal as a guardrail sees it in `proposal`. Every field is computed
/// by the leader from its own state; the author claims none of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProposalFacts {
    /// `apply`, `scale`, `rollback`, `stop`, `delete`, `restart`, `cancel_deploy`,
    /// `clear_fault` or `undo`.
    pub kind: String,
    /// The one namespace every touched app is in.
    pub namespace: String,
    /// The touched apps' names, without their namespace.
    pub apps: Vec<String>,
    /// The nodes those apps are placed on now.
    pub nodes: Vec<String>,
    /// Every instance it replaces, starts or stops.
    pub instances: u32,
    /// For a `scale`, the replica count before; 0 for any other kind.
    pub replicas_from: u32,
    /// For a `scale`, the replica count after; 0 for any other kind.
    pub replicas_to: u32,
    /// `exact`, `lossy` or `none`: how completely its undo puts things back.
    pub reversibility: String,
    /// The changed field paths, such as `replicas` or `env.LOG_LEVEL`,
    /// sorted, each once.
    pub fields: Vec<String>,
    /// The images in the `after` specs, sorted, each once.
    pub images: Vec<String>,
}

/// Who made a proposal, as a guardrail sees it in `author`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthorFacts {
    /// The token's name.
    pub name: String,
    /// `agent` or `human`.
    pub kind: String,
    /// `read-only`, `deployer` or `admin`.
    pub role: String,
}

/// Everything a guardrail reads about a proposal: `proposal` and `author`.
/// `relish proposal show -o json` and `relish agent propose` carry it as
/// `policy_input`, and `relish policy test --proposal` judges it offline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyInput {
    /// The change.
    pub proposal: ProposalFacts,
    /// Who asked for it.
    pub author: AuthorFacts,
}

/// What the guardrails made of one proposal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProposalJudgement {
    /// What the `deny`, `warn` and `audit` guardrails said. A guardrail
    /// that can't be evaluated is a finding too, so a broken `deny` fails
    /// closed.
    pub verdict: Verdict,
    /// The first `approve` guardrail, in name order, whose `match` is true.
    /// The built-in rules can still hold the proposal for a person.
    pub approve: Option<String>,
}

/// Judge `input` by every policy in `set` that applies to proposals, as
/// `caller`. App, job and workload policies are skipped: they judge the
/// proposal's `after` specs, elsewhere.
pub fn judge_proposal_input(
    set: &PolicySet,
    input: &PolicyInput,
    caller: &Caller,
) -> ProposalJudgement {
    let mut judgement = ProposalJudgement::default();
    let guardrails: Vec<_> = set
        .policies
        .iter()
        .filter(|policy| policy.target == PolicyTarget::Proposals)
        .collect();
    if guardrails.is_empty() {
        return judgement;
    }
    let context = proposal_context(set, input, caller);
    let workload = AppId::new(input.proposal.apps.join(","), &input.proposal.namespace);
    let finding = |policy: &str, action, message| Finding {
        policy: policy.to_string(),
        action,
        kind: WorkloadKind::Proposal,
        workload: workload.clone(),
        message,
    };
    for policy in guardrails {
        if policy.action == PolicyAction::Approve {
            // A `match` that can't be evaluated approves nothing; it's
            // recorded, so an admin can see why nothing was approved.
            match matches(policy, &context) {
                Ok(true) if judgement.approve.is_none() => {
                    judgement.approve = Some(policy.name.clone());
                }
                Ok(_) => {}
                Err(reason) => judgement.verdict.audits.push(finding(
                    &policy.name,
                    PolicyAction::Audit,
                    format!("couldn't evaluate: {reason}"),
                )),
            }
            continue;
        }
        let Some(deny) = &policy.deny_program else {
            continue;
        };
        let message = match decide(policy, deny, &context) {
            Ok(false) => continue,
            Ok(true) => policy
                .message
                .clone()
                .unwrap_or_else(|| format!("breaks policy {}", policy.name)),
            Err(reason) => format!("couldn't evaluate: {reason}"),
        };
        judgement
            .verdict
            .push(finding(&policy.name, policy.action, message));
    }
    judgement
}

/// The variables a guardrail sees: `proposal`, `author`, `caller` and
/// `source`, which is always `proposal`.
fn proposal_context(
    set: &PolicySet,
    input: &PolicyInput,
    caller: &Caller,
) -> Context<'static, 'static> {
    let mut context = Context::with_env(Arc::clone(&set.environment));
    let as_cel = |value: serde_json::Result<serde_json::Value>| {
        json_to_cel(value.unwrap_or(serde_json::Value::Null))
    };
    context.add_variable_from_value("proposal", as_cel(serde_json::to_value(&input.proposal)));
    context.add_variable_from_value("author", as_cel(serde_json::to_value(&input.author)));
    context.add_variable_from_value(
        "caller",
        HashMap::from([
            ("name".to_string(), Value::from(caller.name.clone())),
            ("role".to_string(), Value::from(caller.role_name())),
        ]),
    );
    context.add_variable_from_value("source", Source::Proposal.as_str());
    context
}

#[cfg(test)]
#[path = "proposal_tests.rs"]
mod tests;
