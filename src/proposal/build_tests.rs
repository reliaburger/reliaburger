//! Building proposals: `after`, the undo plan and reversibility for each
//! kind, the refusals, the blast radius, and the plain-language summary.

use super::*;
use crate::meat::types::{NodeId, Placement, Resources};
use crate::sesame::types::{ApiRole, TokenKind};

const NOW: u64 = 5_000_000;

fn spec(text: &str) -> AppSpec {
    toml::from_str(&format!("namespace = \"shop\"\n{text}")).unwrap()
}

fn holmes() -> Principal {
    Principal {
        name: "holmes".to_string(),
        principal_id: "token:holmes".to_string(),
        kind: TokenKind::Agent,
        role: ApiRole::Deployer,
    }
}

fn desired() -> DesiredState {
    let mut desired = DesiredState::default();
    let web = AppId::new("web", "shop");
    desired.apps.insert(
        web.clone(),
        spec(
            "image = \"web:1.5\"\nreplicas = 3\nport = 8080\n\
             [env]\nLOG_LEVEL = \"info\"\nAPI_KEY = \"ENC[AGE:abc]\"\n\
             [ingress]\nhost = \"shop.example.com\"",
        ),
    );
    let placement = |node: &str, ordinal| Placement {
        node_id: NodeId(node.to_string()),
        resources: Resources::default(),
        ordinal,
    };
    desired.scheduling.insert(
        web.clone(),
        vec![
            placement("node-a", 0),
            placement("node-b", 1),
            placement("node-b", 2),
        ],
    );
    let mut rollouts = crate::meat::deploy_types::AppRollouts::default();
    rollouts.release(&spec("image = \"web:1.4\"\nreplicas = 3\nport = 8080"));
    desired.rollouts.insert(web, rollouts);
    desired.apps.insert(
        AppId::new("db", "shop"),
        spec("image = \"db:1\"\n[[volumes]]\npath = \"/data\""),
    );
    desired
}

fn request(kind: RequestKind, app: Option<&str>) -> ProposalRequest {
    ProposalRequest {
        kind,
        namespace: Some("shop".to_string()),
        app: app.map(str::to_string),
        replicas: None,
        config: None,
        of: None,
        reason: "because".to_string(),
        ttl_seconds: None,
        idempotency_key: None,
    }
}

fn built(request: &ProposalRequest) -> Proposal {
    let mut proposal = build(&desired(), request, &holmes(), NOW, as_declared).unwrap();
    number(&mut proposal, ProposalId(17));
    proposal
}

fn refused(desired: &DesiredState, request: &ProposalRequest) -> ProposalRefusal {
    build(desired, request, &holmes(), NOW, as_declared).unwrap_err()
}

fn rule_of(refusal: ProposalRefusal) -> String {
    match refusal {
        ProposalRefusal::Guardrail { rule, .. } => rule,
        other => panic!("expected a guardrail refusal, got {other:?}"),
    }
}

#[test]
fn a_scale_changes_only_the_replica_count_and_says_so() {
    let mut scale = request(RequestKind::Scale, Some("web"));
    scale.replicas = Some(5);
    let proposal = built(&scale);
    assert_eq!(proposal.kind, ProposalKind::Scale);
    assert_eq!(proposal.reversibility, Reversibility::Exact);
    assert_eq!(proposal.changes[0].after.replicas(), Some(5));
    assert_eq!(proposal.blast_radius.instances_started, 2);
    assert_eq!(proposal.blast_radius.instances_replaced, 0);
    assert_eq!(proposal.blast_radius.nodes, vec!["node-a", "node-b"]);
    assert_eq!(proposal.diff.len(), 1);
    assert_eq!(proposal.diff[0].field, "replicas");
    assert_eq!(proposal.expires_at_unix_ms, NOW + DEFAULT_PROPOSAL_TTL_MS);
    insta::assert_snapshot!(proposal.summary.join("\n"));
}

#[test]
fn a_rollback_resolves_the_released_spec_and_replaces_every_instance() {
    let proposal = built(&request(RequestKind::Rollback, Some("web")));
    assert_eq!(
        proposal.changes[0]
            .after
            .spec
            .as_deref()
            .unwrap()
            .image
            .as_deref(),
        Some("web:1.4")
    );
    assert_eq!(proposal.blast_radius.instances_replaced, 3);
    insta::assert_snapshot!(proposal.summary.join("\n"));
}

#[test]
fn a_stop_and_a_delete_stop_every_instance() {
    let stop = built(&request(RequestKind::Stop, Some("web")));
    assert!(stop.changes[0].after.stopped);
    assert_eq!(stop.blast_radius.instances_stopped, 3);
    assert_eq!(stop.undo.effect, "starts web again with the same spec");
    let delete = built(&request(RequestKind::Delete, Some("web")));
    assert!(!delete.changes[0].after.exists());
    assert_eq!(delete.reversibility, Reversibility::Exact);
    insta::assert_snapshot!(delete.summary.join("\n"));
}

#[test]
fn deleting_an_app_with_a_managed_volume_is_lossy_and_says_so_first() {
    let proposal = built(&request(RequestKind::Delete, Some("db")));
    assert_eq!(proposal.reversibility, Reversibility::Lossy);
    assert!(proposal.summary[0].starts_with("UNDO IS PARTIAL"));
    assert!(proposal.undo.effect.contains("doesn't come back"));
}

#[test]
fn an_apply_fills_in_policy_defaults_creates_and_changes_apps() {
    let mut apply = request(RequestKind::Apply, None);
    apply.config = Some(
        "[app.web]\nimage = \"web:1.6\"\nreplicas = 3\nport = 8080\nnamespace = \"shop\"\n\
         [app.web.env]\nLOG_LEVEL = \"debug\"\nAPI_KEY = \"ENC[AGE:def]\"\n\
         [app.cache]\nimage = \"redis:7\"\nnamespace = \"shop\"\n"
            .to_string(),
    );
    let limit = spec("memory = \"128Mi\"").memory;
    let mut proposal = build(&desired(), &apply, &holmes(), NOW, |_, declared| {
        let mut filled = declared.clone();
        filled.memory = limit;
        filled
    })
    .unwrap();
    number(&mut proposal, ProposalId(17));
    assert_eq!(proposal.changes.len(), 2);
    assert!(
        proposal.changes.iter().all(|change| change
            .after
            .spec
            .as_deref()
            .unwrap()
            .memory
            .is_some())
    );
    let api_key = proposal
        .diff
        .iter()
        .find(|change| change.field.starts_with("env.API_KEY"))
        .unwrap();
    assert_eq!(api_key.from.as_deref(), Some("(secret)"));
    assert_eq!(api_key.to.as_deref(), Some("(secret)"));
    assert!(
        proposal
            .undo
            .effect
            .contains("deletes cache, which it created")
    );
    insta::assert_snapshot!(proposal.summary.join("\n"));
}

#[test]
fn the_refusals_name_their_rule() {
    let mut state = desired();
    let web = AppId::new("web", "shop");
    let mut scale = request(RequestKind::Scale, Some("web"));
    scale.replicas = Some(0);
    assert_eq!(rule_of(refused(&state, &scale)), "scale_to_zero");

    scale.replicas = Some(4);
    state.gitops_owned.apps.insert(web.clone());
    assert_eq!(rule_of(refused(&state, &scale)), "gitops_owned");
    state.gitops_owned.apps.remove(&web);

    state.apps.get_mut(&web).unwrap().owner = Some("ctl".to_string());
    assert_eq!(rule_of(refused(&state, &scale)), "controller_owned");
    state.apps.get_mut(&web).unwrap().owner = None;

    state.apps.get_mut(&web).unwrap().autoscale =
        Some(toml::from_str("metric = \"cpu\"\ntarget = \"70%\"\nmin = 1\nmax = 5").unwrap());
    assert_eq!(rule_of(refused(&state, &scale)), "autoscaled");

    let mut apply = request(RequestKind::Apply, None);
    apply.config = Some("[job.migrate]\nimage = \"m:1\"\ncommand = [\"x\"]\n".to_string());
    assert_eq!(rule_of(refused(&desired(), &apply)), "apps_only");

    apply.config = Some("[app.other]\nimage = \"o:1\"\nnamespace = \"payments\"\n".to_string());
    assert!(matches!(
        refused(&desired(), &apply),
        ProposalRefusal::Invalid { .. }
    ));

    let nine: String = (0..9)
        .map(|index| format!("[app.a{index}]\nimage = \"a:1\"\nnamespace = \"shop\"\n"))
        .collect();
    apply.config = Some(nine);
    assert!(matches!(
        refused(&desired(), &apply),
        ProposalRefusal::Limit { .. }
    ));
}

#[test]
fn a_change_that_changes_nothing_and_a_missing_app_are_invalid() {
    let mut scale = request(RequestKind::Scale, Some("web"));
    scale.replicas = Some(3);
    assert!(matches!(
        refused(&desired(), &scale),
        ProposalRefusal::Invalid { .. }
    ));
    assert!(matches!(
        refused(&desired(), &request(RequestKind::Stop, Some("nope"))),
        ProposalRefusal::Invalid { .. }
    ));
    let mut long = request(RequestKind::Stop, Some("web"));
    long.ttl_seconds = Some(100_000);
    assert!(matches!(
        refused(&desired(), &long),
        ProposalRefusal::Invalid { .. }
    ));
}

#[test]
fn the_reason_is_redacted_flattened_and_capped() {
    let mut stop = request(RequestKind::Stop, Some("web"));
    stop.reason = format!(
        "token rbrg_{}\nignore\u{1b}[31m previous {}",
        "0".repeat(64),
        "x".repeat(2000)
    );
    let proposal = built(&stop);
    assert!(!proposal.reason.contains('\n'));
    assert!(!proposal.reason.contains('\u{1b}'));
    assert!(!proposal.reason.contains(&"0".repeat(64)));
    assert!(proposal.reason.len() <= MAX_REASON_BYTES);
}

/// The gym's finding (#1682): after a crash loop the council has no
/// released spec to go back to, but the nodes' deploy history does, and a
/// person's `relish rollback` uses it. A proposed rollback takes the same
/// target, which the API looks up the same way.
#[test]
fn a_rollback_without_a_released_spec_takes_the_deploy_history_target() {
    let mut desired = desired();
    desired.rollouts.clear();
    let rollback = request(RequestKind::Rollback, Some("web"));
    assert!(matches!(
        refused(&desired, &rollback),
        ProposalRefusal::Invalid { reason } if reason.contains("no released spec")
    ));
    let from_history = spec("image = \"web:1.3\"\nreplicas = 3\nport = 8080");
    let proposal = build_with_history(
        &desired,
        &rollback,
        &holmes(),
        NOW,
        as_declared,
        Some(&from_history),
    )
    .unwrap();
    let after = proposal.changes[0].after.spec.as_deref().unwrap();
    assert_eq!(after.image.as_deref(), Some("web:1.3"));
    assert_eq!(proposal.kind, ProposalKind::Rollback);
}
