//! Guardrails (#1682): what a policy that applies to proposals may say,
//! what it sees, and what it decides.

use std::collections::BTreeMap;

use super::*;
use crate::config::{Config, PolicySpec};
use crate::policy::{PolicyError, check_policy, evaluate_app};
use crate::sesame::types::ApiRole;

/// The four guardrails `examples/agents/guardrails.toml` ships.
const EXAMPLE: &str = include_str!("../../examples/agents/guardrails.toml");

fn policies(toml: &str) -> BTreeMap<String, PolicySpec> {
    Config::parse(toml).unwrap().policy
}

fn compile(toml: &str) -> PolicySet {
    PolicySet::compile(&policies(toml)).unwrap()
}

fn input(kind: &str, namespace: &str, author_kind: &str) -> PolicyInput {
    PolicyInput {
        proposal: ProposalFacts {
            kind: kind.to_string(),
            namespace: namespace.to_string(),
            apps: vec!["web".to_string()],
            nodes: vec!["node-a".to_string()],
            instances: 2,
            replicas_from: 3,
            replicas_to: 5,
            reversibility: "exact".to_string(),
            fields: vec!["replicas".to_string()],
            images: vec!["web:1.4".to_string()],
        },
        author: AuthorFacts {
            name: "holmes".to_string(),
            kind: author_kind.to_string(),
            role: "deployer".to_string(),
        },
    }
}

/// Whether an error is the one a case expects.
type ErrorCheck = fn(&PolicyError) -> bool;

fn holmes() -> Caller {
    Caller::token("holmes", ApiRole::Deployer)
}

fn check(toml: &str) -> Result<(), PolicyError> {
    let policies = policies(toml);
    let (name, spec) = policies.iter().next().unwrap();
    check_policy(name, spec)
}

#[test]
fn a_guardrail_sees_proposal_author_caller_and_source_only() {
    let ok = r#"
        [policy.rule]
        applies_to = "proposals"
        match = 'author.kind == "agent" && caller.role == "deployer" && source == "proposal"'
        deny = 'proposal.kind == "delete" || size(proposal.images) > 1 || proposal.replicas_to > 10'
    "#;
    check(ok).unwrap();
    let wrong_variable = r#"
        [policy.rule]
        applies_to = "proposals"
        deny = 'app.image == "x"'
    "#;
    assert!(matches!(
        check(wrong_variable),
        Err(PolicyError::UnknownVariable { variable, .. }) if variable == "app"
    ));
    // The reason is the author's own words: no guardrail reads it.
    let reason = r#"
        [policy.rule]
        applies_to = "proposals"
        deny = 'proposal.reason.contains("urgent")'
    "#;
    assert!(matches!(
        check(reason),
        Err(PolicyError::UnknownField { variable, name, .. })
            if variable == "proposal" && name == "reason"
    ));
    let author_field = r#"
        [policy.rule]
        applies_to = "proposals"
        deny = 'author.token == "x"'
    "#;
    assert!(matches!(
        check(author_field),
        Err(PolicyError::UnknownField { variable, .. }) if variable == "author"
    ));
    // An app policy can't read a proposal.
    let app_policy = "[policy.rule]\ndeny = 'proposal.kind == \"delete\"'";
    assert!(matches!(
        check(app_policy),
        Err(PolicyError::UnknownVariable { variable, .. }) if variable == "proposal"
    ));
}

#[test]
fn the_documented_fields_are_the_facts_a_guardrail_gets() {
    let fields = |value: serde_json::Value| -> Vec<String> {
        value.as_object().unwrap().keys().cloned().collect()
    };
    let mut proposal: Vec<&str> = crate::policy::PROPOSAL_FIELDS.to_vec();
    proposal.sort_unstable();
    assert_eq!(
        fields(serde_json::to_value(ProposalFacts::default()).unwrap()),
        proposal
    );
    let mut author: Vec<&str> = crate::policy::AUTHOR_FIELDS.to_vec();
    author.sort_unstable();
    assert_eq!(
        fields(serde_json::to_value(AuthorFacts::default()).unwrap()),
        author
    );
}

#[test]
fn a_guardrail_needs_a_deny_and_takes_no_default() {
    assert!(matches!(
        check("[policy.rule]\napplies_to = 'proposals'\nmatch = 'true'"),
        Err(PolicyError::NothingToDo)
    ));
    assert!(matches!(
        check(
            "[policy.rule]\napplies_to = 'proposals'\ndeny = 'true'\n\
             [policy.rule.default]\nmemory = '1Gi'"
        ),
        Err(PolicyError::DefaultOutsideApps { .. })
    ));
}

#[test]
fn an_approve_policy_has_a_match_and_nothing_else() {
    check("[policy.ok]\napplies_to = 'proposals'\naction = 'approve'\nmatch = 'true'").unwrap();
    let cases: [(&str, ErrorCheck); 5] = [
        ("action = 'approve'\nmatch = 'true'", |e| {
            matches!(e, PolicyError::ApproveOutsideProposals { .. })
        }),
        (
            "applies_to = 'jobs'\naction = 'approve'\nmatch = 'true'",
            |e| matches!(e, PolicyError::ApproveOutsideProposals { .. }),
        ),
        (
            "applies_to = 'proposals'\naction = 'approve'\nmatch = 'true'\ndeny = 'true'",
            |e| matches!(e, PolicyError::ApproveTakesOnlyMatch { field: "deny" }),
        ),
        (
            "applies_to = 'proposals'\naction = 'approve'\nmatch = 'true'\nmessage = 'hi'",
            |e| matches!(e, PolicyError::ApproveTakesOnlyMatch { field: "message" }),
        ),
        ("applies_to = 'proposals'\naction = 'approve'", |e| {
            matches!(e, PolicyError::ApproveNeedsMatch)
        }),
    ];
    for (body, expected) in cases {
        let error = check(&format!("[policy.rule]\n{body}")).unwrap_err();
        assert!(expected(&error), "{body}: {error}");
    }
}

#[test]
fn the_example_guardrails_refuse_and_approve_what_they_say() {
    let set = compile(EXAMPLE);
    // An agent's delete is refused, naming the policy.
    let judged = judge_proposal_input(&set, &input("delete", "shop", "agent"), &holmes());
    assert_eq!(judged.verdict.denials.len(), 1);
    assert_eq!(judged.verdict.denials[0].policy, "agents-never-delete");
    assert_eq!(judged.verdict.denials[0].kind, WorkloadKind::Proposal);
    assert_eq!(
        judged.verdict.denials[0].message,
        "agents may scale and roll back, never delete"
    );
    // A person's isn't.
    let judged = judge_proposal_input(&set, &input("delete", "shop", "human"), &holmes());
    assert!(judged.verdict.is_allowed());
    assert_eq!(judged.approve, None);
    // An agent keeps out of payments.
    let judged = judge_proposal_input(&set, &input("scale", "payments", "agent"), &holmes());
    assert_eq!(
        judged.verdict.denials[0].policy,
        "agents-keep-out-of-payments"
    );
    // A small scale in shop is approved; a big one isn't.
    let judged = judge_proposal_input(&set, &input("scale", "shop", "agent"), &holmes());
    assert!(judged.verdict.is_empty());
    assert_eq!(judged.approve.as_deref(), Some("agents-scale-shop"));
    let mut big = input("scale", "shop", "agent");
    big.proposal.replicas_to = 11;
    assert_eq!(judge_proposal_input(&set, &big, &holmes()).approve, None);
}

#[test]
fn the_first_approve_policy_by_name_approves_and_a_broken_one_approves_nothing() {
    let set = compile(
        r#"
        [policy.b-second]
        applies_to = "proposals"
        action = "approve"
        match = 'proposal.kind == "scale"'

        [policy.a-first]
        applies_to = "proposals"
        action = "approve"
        match = 'proposal.kind == "scale"'
        "#,
    );
    let judged = judge_proposal_input(&set, &input("scale", "shop", "agent"), &holmes());
    assert_eq!(judged.approve.as_deref(), Some("a-first"));

    // `match` returns a string, not a boolean: it can't be evaluated.
    let set = compile(
        r#"
        [policy.broken]
        applies_to = "proposals"
        action = "approve"
        match = 'proposal.kind'
        "#,
    );
    let judged = judge_proposal_input(&set, &input("scale", "shop", "agent"), &holmes());
    assert_eq!(judged.approve, None);
    assert_eq!(judged.verdict.audits.len(), 1);
    assert!(
        judged.verdict.audits[0]
            .message
            .starts_with("couldn't evaluate")
    );
}

#[test]
fn warn_and_audit_guardrails_are_findings_and_a_broken_deny_refuses() {
    let set = compile(
        r#"
        [policy.many-instances]
        applies_to = "proposals"
        action = "warn"
        deny = 'proposal.instances > 1'
        message = "more than one instance"

        [policy.note-agents]
        applies_to = "proposals"
        action = "audit"
        deny = 'author.kind == "agent"'

        [policy.broken]
        applies_to = "proposals"
        deny = 'proposal.kind'
        "#,
    );
    let judged = judge_proposal_input(&set, &input("scale", "shop", "agent"), &holmes());
    assert_eq!(judged.verdict.warnings[0].policy, "many-instances");
    assert_eq!(judged.verdict.audits[0].policy, "note-agents");
    assert_eq!(judged.verdict.denials[0].policy, "broken");
}

#[test]
fn guardrails_and_app_policies_judge_only_their_own_kind() {
    let set = compile(
        r#"
        [policy.no-delete]
        applies_to = "proposals"
        deny = 'proposal.kind == "delete"'

        [policy.app-rule]
        deny = 'app.image == "web:1.4"'
        "#,
    );
    // The app rule doesn't judge a proposal...
    let judged = judge_proposal_input(&set, &input("scale", "shop", "agent"), &holmes());
    assert!(judged.verdict.is_empty());
    // ...and the guardrail doesn't judge an app.
    let app: crate::config::AppSpec = toml::from_str("image = \"web:1.5\"").unwrap();
    let verdict = evaluate_app(
        &set,
        &AppId::new("web", "shop"),
        &app,
        &holmes(),
        Source::Apply,
    );
    assert!(verdict.is_empty());
}
