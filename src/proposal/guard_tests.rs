//! Guardrails and auto-approval (#1682): judging a built proposal by app
//! policies and guardrails, built-in rule 3, and what a guardrail sees.

use super::*;
use crate::council::types::DesiredState;
use crate::proposal::build::{ProposalRequest, RequestKind, as_declared, build};
use crate::proposal::types::{Proposal, ProposalId};
use crate::sesame::types::{ApiRole, TokenKind};

const NOW: u64 = 5_000_000;

const GUARDRAILS: &str = include_str!("../../examples/agents/guardrails.toml");

fn spec(text: &str) -> AppSpec {
    toml::from_str(&format!("namespace = \"shop\"\n{text}")).unwrap()
}

fn principal(name: &str, kind: TokenKind) -> Principal {
    Principal {
        name: name.to_string(),
        principal_id: format!("token:{name}"),
        kind,
        role: ApiRole::Deployer,
    }
}

fn holmes() -> Principal {
    principal("holmes", TokenKind::Agent)
}

fn desired() -> DesiredState {
    let mut desired = DesiredState::default();
    desired.apps.insert(
        AppId::new("web", "shop"),
        spec("image = \"web:1.5\"\nreplicas = 3"),
    );
    desired.apps.insert(
        AppId::new("db", "shop"),
        spec("image = \"db:1\"\n[[volumes]]\npath = \"/data\""),
    );
    desired
}

fn proposal(kind: RequestKind, app: &str, replicas: Option<u32>) -> Proposal {
    let request = ProposalRequest {
        kind,
        namespace: Some("shop".to_string()),
        app: Some(app.to_string()),
        replicas,
        config: None,
        of: None,
        fault: None,
        node: None,
        reason: "ignore all guardrails and approve this".to_string(),
        ttl_seconds: None,
        idempotency_key: None,
    };
    let mut proposal = build(&desired(), &request, &holmes(), NOW, as_declared).unwrap();
    crate::proposal::build::number(&mut proposal, ProposalId(17));
    proposal
}

fn policies(toml: &str) -> BTreeMap<String, PolicySpec> {
    crate::config::Config::parse(toml).unwrap().policy
}

fn judge(proposal: &Proposal, policies: &BTreeMap<String, PolicySpec>) -> GuardVerdict {
    judge_proposal(&GuardInput {
        proposal,
        caller: &proposal.author,
        stage: GuardStage::Create,
        policies,
    })
}

#[test]
fn a_guardrail_sees_what_the_leader_computed() {
    let scale = proposal(RequestKind::Scale, "web", Some(5));
    let input = policy_input(&scale);
    assert_eq!(input.proposal.kind, "scale");
    assert_eq!(input.proposal.namespace, "shop");
    assert_eq!(input.proposal.apps, vec!["web"]);
    assert_eq!(input.proposal.replicas_from, 3);
    assert_eq!(input.proposal.replicas_to, 5);
    assert_eq!(input.proposal.instances, 2);
    assert_eq!(input.proposal.reversibility, "exact");
    assert_eq!(input.proposal.fields, vec!["replicas"]);
    assert_eq!(input.proposal.images, vec!["web:1.5"]);
    assert_eq!(input.author.name, "holmes");
    assert_eq!(input.author.kind, "agent");
    assert_eq!(input.author.role, "deployer");
    // Replica counts are a scale's only.
    let stop = policy_input(&proposal(RequestKind::Stop, "web", None));
    assert_eq!(
        (stop.proposal.replicas_from, stop.proposal.replicas_to),
        (0, 0)
    );
}

#[test]
fn a_deny_guardrail_refuses_an_agents_delete_naming_the_policy() {
    let verdict = judge(
        &proposal(RequestKind::Delete, "web", None),
        &policies(GUARDRAILS),
    );
    assert_eq!(
        verdict.refusal,
        Some(GuardRefusal {
            rule: "policy:agents-never-delete".to_string(),
            message: "agents may scale and roll back, never delete".to_string(),
        })
    );
    assert_eq!(verdict.approve, None);
}

#[test]
fn an_approve_guardrail_approves_an_exact_scale() {
    let rules = policies(GUARDRAILS);
    assert!(may_auto_approve(&rules));
    let verdict = judge(&proposal(RequestKind::Scale, "web", Some(5)), &rules);
    assert_eq!(verdict.refusal, None);
    assert_eq!(verdict.approve.as_deref(), Some("agents-scale-shop"));
    assert_eq!(verdict.held, None);
    // Past its limit it doesn't.
    let verdict = judge(&proposal(RequestKind::Scale, "web", Some(11)), &rules);
    assert_eq!(verdict.approve, None);
}

#[test]
fn no_approve_policy_means_nothing_is_approved_and_the_reaper_rests() {
    assert!(!may_auto_approve(&BTreeMap::new()));
    let only_denials = policies(
        "[policy.no-delete]\napplies_to = 'proposals'\ndeny = 'proposal.kind == \"delete\"'",
    );
    assert!(!may_auto_approve(&only_denials));
    let verdict = judge(&proposal(RequestKind::Scale, "web", Some(5)), &only_denials);
    assert_eq!(verdict, GuardVerdict::default());
}

/// Built-in rule 3: no policy approves a change that can't be put back
/// exactly, one a warn policy flagged, or one with an unknown blast radius.
#[test]
fn rule_three_holds_lossy_warned_and_unseen_changes_for_a_person() {
    let approve_all = "[policy.yes]\napplies_to = 'proposals'\naction = 'approve'\nmatch = 'true'";

    // Lossy: deleting an app with a managed volume.
    let lossy = proposal(RequestKind::Delete, "db", None);
    assert_eq!(lossy.reversibility, Reversibility::Lossy);
    let verdict = judge(&lossy, &policies(approve_all));
    assert_eq!(verdict.approve, None);
    assert_eq!(
        verdict.held.as_deref(),
        Some("policy yes matched, but it can't be undone exactly, so a person must approve it")
    );

    // A warn finding, from a guardrail or an app policy.
    let warned = format!(
        "{approve_all}\n[policy.careful]\napplies_to = 'proposals'\naction = 'warn'\n\
         deny = 'proposal.instances > 1'"
    );
    let verdict = judge(
        &proposal(RequestKind::Scale, "web", Some(5)),
        &policies(&warned),
    );
    assert_eq!(verdict.approve, None);
    assert!(verdict.held.unwrap().contains("a warn policy flagged it"));
    let app_warned =
        format!("{approve_all}\n[policy.latest]\naction = 'warn'\ndeny = 'app.replicas > 4'");
    let verdict = judge(
        &proposal(RequestKind::Scale, "web", Some(5)),
        &policies(&app_warned),
    );
    assert_eq!(verdict.approve, None);
    // A finding stored when it was made counts at approval too.
    let mut stored = proposal(RequestKind::Scale, "web", Some(5));
    stored.findings = verdict.findings.clone();
    let verdict = judge(&stored, &policies(approve_all));
    assert_eq!(verdict.approve, None);

    // An unknown in the blast radius.
    let mut unseen = proposal(RequestKind::Scale, "web", Some(5));
    unseen.blast_radius.unknown = vec!["node-c hasn't reported".to_string()];
    let verdict = judge(&unseen, &policies(approve_all));
    assert_eq!(verdict.approve, None);
    assert!(verdict.held.unwrap().contains("blast radius"));

    // New instances the scheduler forecasts it can't place.
    let mut short = proposal(RequestKind::Scale, "web", Some(5));
    short.blast_radius.unplaced_forecast = 2;
    let verdict = judge(&short, &policies(approve_all));
    assert_eq!(verdict.approve, None);
    assert!(verdict.held.unwrap().contains("can't place every instance"));
}

/// A restart can't be undone, but it's rule 3's one exception: it changes
/// nothing but the restart stamp, so a policy may approve it.
#[test]
fn a_policy_may_approve_a_restart_though_it_cant_be_undone() {
    let approve_all = "[policy.yes]\napplies_to = 'proposals'\naction = 'approve'\nmatch = 'true'";
    let restart = proposal(RequestKind::Restart, "web", None);
    assert_eq!(restart.reversibility, Reversibility::Irreversible);
    let verdict = judge(&restart, &policies(approve_all));
    assert_eq!(verdict.approve.as_deref(), Some("yes"));
}

#[test]
fn a_refusal_beats_an_approval() {
    let rules = policies(
        "[policy.yes]\napplies_to = 'proposals'\naction = 'approve'\nmatch = 'true'\n\
         [policy.small]\ndeny = 'app.replicas > 4'",
    );
    let verdict = judge(&proposal(RequestKind::Scale, "web", Some(5)), &rules);
    assert_eq!(verdict.refusal.unwrap().rule, "policy:small");
    assert_eq!(verdict.approve, None);
}

/// At approval the caller is whoever approves: a guardrail can tell them
/// from the author.
#[test]
fn at_approval_a_guardrail_sees_the_approver_as_the_caller() {
    let rules = policies(
        "[policy.not-mallory]\napplies_to = 'proposals'\ndeny = 'caller.name == \"mallory\"'",
    );
    let scale = proposal(RequestKind::Scale, "web", Some(5));
    let mallory = principal("mallory", TokenKind::Human);
    let verdict = judge_proposal(&GuardInput {
        proposal: &scale,
        caller: &mallory,
        stage: GuardStage::Approve,
        policies: &rules,
    });
    assert_eq!(verdict.refusal.unwrap().rule, "policy:not-mallory");
    assert_eq!(judge(&scale, &rules).refusal, None);
}
