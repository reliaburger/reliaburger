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
        fault: None,
        node: None,
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

/// A restart stamps `restarted_at` and changes nothing else: the rollout
/// replaces every instance through the health checks. It can't be undone,
/// and the summary says so first.
#[test]
fn a_restart_stamps_the_spec_and_cant_be_undone() {
    let proposal = built(&request(RequestKind::Restart, Some("web")));
    assert_eq!(proposal.kind, ProposalKind::Restart);
    assert_eq!(proposal.reversibility, Reversibility::Irreversible);
    assert_eq!(proposal.undo.command, None);
    let after = proposal.changes[0].after.spec.as_deref().unwrap();
    assert_eq!(after.restarted_at, Some(NOW));
    assert_eq!(proposal.diff.len(), 1);
    assert_eq!(proposal.diff[0].field, "restarted_at");
    assert_eq!(proposal.blast_radius.instances_replaced, 3);
    assert!(proposal.summary[0].starts_with("CAN'T BE UNDONE"));
    insta::assert_snapshot!(proposal.summary.join("\n"));

    // A second restart moves the stamp on even if the clock went back.
    let mut state = desired();
    let web = AppId::new("web", "shop");
    state.apps.get_mut(&web).unwrap().restarted_at = Some(NOW + 50);
    let again = build(
        &state,
        &request(RequestKind::Restart, Some("web")),
        &holmes(),
        NOW,
        as_declared,
    )
    .unwrap();
    let after = again.changes[0].after.spec.as_deref().unwrap();
    assert_eq!(after.restarted_at, Some(NOW + 51));

    state.stopped_apps.insert(web);
    assert!(matches!(
        refused(&state, &request(RequestKind::Restart, Some("web"))),
        ProposalRefusal::Invalid { reason } if reason.contains("stopped")
    ));
}

/// Cancelling a rollout puts the app back on the newest spec that finished
/// rolling out everywhere, at its current scale. Its undo is exact: the new
/// spec comes back and its rollout starts again.
#[test]
fn cancelling_a_rollout_goes_back_to_the_last_released_spec() {
    let proposal = built(&request(RequestKind::CancelDeploy, Some("web")));
    assert_eq!(proposal.kind, ProposalKind::CancelDeploy);
    assert_eq!(proposal.reversibility, Reversibility::Exact);
    let after = proposal.changes[0].after.spec.as_deref().unwrap();
    assert_eq!(after.image.as_deref(), Some("web:1.4"));
    assert_eq!(after.replicas, Replicas::Fixed(3));
    assert_eq!(proposal.undo.command.as_deref(), Some("relish undo 17"));
    assert!(proposal.undo.effect.contains("starts its rollout again"));
    insta::assert_snapshot!(proposal.summary.join("\n"));

    // Nothing rolling out: the current spec is the last released one.
    let mut state = desired();
    let web = AppId::new("web", "shop");
    let current = state.apps[&web].clone();
    state.rollouts.get_mut(&web).unwrap().release(&current);
    assert!(matches!(
        refused(&state, &request(RequestKind::CancelDeploy, Some("web"))),
        ProposalRefusal::Invalid { reason } if reason.contains("no rollout in progress")
    ));
    // Nothing ever finished rolling out: there's nothing to go back to.
    state.rollouts.clear();
    assert!(matches!(
        refused(&state, &request(RequestKind::CancelDeploy, Some("web"))),
        ProposalRefusal::Invalid { reason } if reason.contains("nothing")
            || reason.contains("no spec that finished")
    ));
}

/// `web` running `current` with `released` as the last spec that finished
/// rolling out, for the summaries that name both ends (#1875), with each
/// spec's short revision.
fn rolled_out(current: &str, released: &str) -> (DesiredState, String, String) {
    let mut state = desired();
    let web = AppId::new("web", "shop");
    let current = spec(current);
    let released = spec(released);
    let short =
        |spec: &AppSpec| super::super::revision::revision_of(Some(spec), false)[..12].to_string();
    let names = (short(&current), short(&released));
    state.apps.insert(web.clone(), current);
    let mut rollouts = crate::meat::deploy_types::AppRollouts::default();
    rollouts.release(&released);
    state.rollouts.insert(web, rollouts);
    (state, names.0, names.1)
}

fn built_from(state: &DesiredState, kind: RequestKind) -> Proposal {
    let mut proposal = build(
        state,
        &request(kind, Some("web")),
        &holmes(),
        NOW,
        as_declared,
    )
    .unwrap();
    number(&mut proposal, ProposalId(17));
    proposal
}

/// An app with no image (a host `exec` workload) names each end of a
/// rollback by its revision, not "its previous spec" twice (#1875).
#[test]
fn a_rollback_of_an_app_with_no_image_names_each_end_by_its_revision() {
    let (state, now, then) = rolled_out(
        "exec = \"/usr/bin/web\"\ncommand = [\"--v2\"]\nreplicas = 1",
        "exec = \"/usr/bin/web\"\ncommand = [\"--v1\"]\nreplicas = 1",
    );
    let proposal = built_from(&state, RequestKind::Rollback);
    assert_eq!(
        proposal.summary[0],
        format!("Roll shop/web back from revision {now} to revision {then}.")
    );
    assert_eq!(
        proposal.undo.effect,
        format!("puts web back on revision {now}")
    );
}

/// Two specs with the same image (only the env changed) name each end by
/// image and revision, so the two ends never read the same.
#[test]
fn a_rollback_between_specs_with_one_image_names_each_end_by_its_revision() {
    let (state, now, then) = rolled_out(
        "image = \"web:1.5\"\n[env]\nLOG_LEVEL = \"debug\"",
        "image = \"web:1.5\"\n[env]\nLOG_LEVEL = \"info\"",
    );
    let proposal = built_from(&state, RequestKind::Rollback);
    assert_eq!(
        proposal.summary[0],
        format!("Roll shop/web back from web:1.5 at revision {now} to web:1.5 at revision {then}.")
    );
}

/// Cancelling a rollout of an app with no image names the spec rolling out
/// and the one it goes back to by revision.
#[test]
fn cancelling_a_rollout_of_an_app_with_no_image_names_each_end_by_its_revision() {
    let (state, now, then) = rolled_out(
        "exec = \"/usr/bin/web\"\ncommand = [\"--v2\"]\nreplicas = 1",
        "exec = \"/usr/bin/web\"\ncommand = [\"--v1\"]\nreplicas = 1",
    );
    let proposal = built_from(&state, RequestKind::CancelDeploy);
    assert_eq!(
        proposal.summary[0],
        format!(
            "Cancel the rollout of shop/web to revision {now}: put it back on revision \
             {then}, the last spec that finished rolling out; nodes already on the new spec \
             roll back."
        )
    );
    assert_eq!(
        proposal.undo.effect,
        format!("puts web back on revision {now}, which starts its rollout again")
    );
}

/// Cancelling a rollout that only changes the env names each end by image
/// and revision.
#[test]
fn cancelling_a_rollout_with_one_image_names_each_end_by_its_revision() {
    let (state, now, then) = rolled_out(
        "image = \"web:1.5\"\n[env]\nLOG_LEVEL = \"debug\"",
        "image = \"web:1.5\"\n[env]\nLOG_LEVEL = \"info\"",
    );
    let proposal = built_from(&state, RequestKind::CancelDeploy);
    assert!(
        proposal.summary[0].starts_with(&format!(
            "Cancel the rollout of shop/web to web:1.5 at revision {now}: put it back on \
             web:1.5 at revision {then}, "
        )),
        "{}",
        proposal.summary[0]
    );
}

/// An apply keeps the stored restart stamp, as `relish apply` does, so
/// applying the same file after a restart restarts nothing.
#[test]
fn an_apply_keeps_the_restart_stamp() {
    let mut state = desired();
    let web = AppId::new("web", "shop");
    state.apps.get_mut(&web).unwrap().restarted_at = Some(42);
    let mut apply = request(RequestKind::Apply, None);
    apply.config = Some(
        "[app.web]\nimage = \"web:1.6\"\nreplicas = 3\nport = 8080\nnamespace = \"shop\"\n"
            .to_string(),
    );
    let proposal = build(&state, &apply, &holmes(), NOW, as_declared).unwrap();
    let after = proposal.changes[0].after.spec.as_deref().unwrap();
    assert_eq!(after.restarted_at, Some(42));
    assert!(
        proposal
            .diff
            .iter()
            .all(|change| change.field != "restarted_at")
    );
}

fn fault() -> FaultTarget {
    FaultTarget {
        id: 7,
        node: "node-b".to_string(),
        app: AppId::new("web", "shop"),
        fault_type: "delay 200ms".to_string(),
        instance: None,
        remaining_secs: 240,
        injected_by: "alice".to_string(),
    }
}

/// Clearing a fault changes no spec: the proposal names the fault and its
/// node, touches the faulted app, and can't be undone.
#[test]
fn clearing_a_fault_names_it_and_changes_no_spec() {
    let mut clear = request(RequestKind::ClearFault, None);
    clear.fault = Some(7);
    let context = BuildContext {
        history_target: None,
        fault: Some(fault()),
    };
    let mut proposal =
        build_with_context(&desired(), &clear, &holmes(), NOW, as_declared, &context).unwrap();
    number(&mut proposal, ProposalId(17));
    assert_eq!(proposal.kind, ProposalKind::ClearFault);
    assert!(proposal.changes.is_empty());
    assert_eq!(proposal.reversibility, Reversibility::Irreversible);
    assert_eq!(proposal.undo.command, None);
    assert_eq!(
        proposal.apps().collect::<Vec<_>>(),
        vec![&AppId::new("web", "shop")]
    );
    assert_eq!(proposal.blast_radius.nodes, vec!["node-b"]);
    assert_eq!(proposal.namespace, "shop");
    insta::assert_snapshot!(proposal.summary.join("\n"));

    // Without the fault the leader found, there's nothing to build.
    assert!(matches!(
        build(&desired(), &clear, &holmes(), NOW, as_declared),
        Err(ProposalRefusal::Invalid { .. })
    ));
    // A GitOps-owned app's fault is a person's to clear, like its spec.
    let mut state = desired();
    state.gitops_owned.apps.insert(AppId::new("web", "shop"));
    let refusal =
        build_with_context(&state, &clear, &holmes(), NOW, as_declared, &context).unwrap_err();
    assert_eq!(rule_of(refusal), "gitops_owned");
}

/// The scheduler's dry run goes into the blast radius and the summary:
/// where new instances would go, and how many it can't place.
#[test]
fn the_forecast_names_where_new_instances_go_and_what_wont_fit() {
    let mut scale = request(RequestKind::Scale, Some("web"));
    scale.replicas = Some(6);
    let mut proposal = build(&desired(), &scale, &holmes(), NOW, as_declared).unwrap();
    with_forecast(
        &mut proposal,
        Forecast {
            nodes: vec![
                "node-c".to_string(),
                "node-a".to_string(),
                "node-c".to_string(),
            ],
            unplaced: 0,
            note: None,
        },
    );
    assert_eq!(
        proposal.blast_radius.new_nodes_forecast,
        vec!["node-a", "node-c"]
    );
    assert!(proposal.summary[1].contains("starts 3 instances, forecast on node-a, node-c."));

    with_forecast(
        &mut proposal,
        Forecast {
            nodes: vec!["node-c".to_string()],
            unplaced: 2,
            note: Some("no node has enough free memory".to_string()),
        },
    );
    assert_eq!(proposal.blast_radius.unplaced_forecast, 2);
    insta::assert_snapshot!(proposal.summary.join("\n"));

    with_forecast(
        &mut proposal,
        Forecast {
            nodes: Vec::new(),
            unplaced: 0,
            note: Some("the leader is still learning what runs where".to_string()),
        },
    );
    assert!(proposal.summary[1].contains(
        "(no forecast: the leader is still learning what runs where; the scheduler places them \
         when it applies)"
    ));
}

/// Recomputed when it's approved, a blast radius that reaches a new node
/// or more instances has grown; one that shrank hasn't.
#[test]
fn a_blast_radius_that_reaches_a_new_node_has_grown() {
    let mut stop = request(RequestKind::Stop, Some("web"));
    stop.reason = "quiet".to_string();
    let proposal = built(&stop);
    let mut state = desired();
    let web = AppId::new("web", "shop");
    assert_eq!(blast_radius_growth(&state, &proposal), None);

    // The scheduler moved a replica off node-b: smaller, not grown.
    state.scheduling.get_mut(&web).unwrap().truncate(2);
    assert_eq!(blast_radius_growth(&state, &proposal), None);

    state.scheduling.get_mut(&web).unwrap().push(Placement {
        node_id: NodeId("node-c".to_string()),
        resources: Resources::default(),
        ordinal: 2,
    });
    assert_eq!(
        blast_radius_growth(&state, &proposal).as_deref(),
        Some("its blast radius grew after it was made: it now touches node-c too")
    );
}
