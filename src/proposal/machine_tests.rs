//! The proposal state machine: every transition, every refusal, stale
//! revisions, leases, undo, expiry and compaction (#1682).

use super::*;
use crate::config::app::AppSpec;
use crate::council::types::DesiredState;
use crate::proposal::build::{ProposalRequest, RequestKind, as_declared, build};
use crate::proposal::lease::{DEFAULT_LEASE_TTL_MS, LeaseTarget};
use crate::proposal::revision::current_revision;
use crate::sesame::types::ApiRole;

const NOW: u64 = 1_000_000;

fn spec(image: &str, replicas: u32) -> AppSpec {
    toml::from_str(&format!(
        "image = {image:?}\nreplicas = {replicas}\nnamespace = \"shop\""
    ))
    .unwrap()
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

fn alice() -> Principal {
    principal("alice", TokenKind::Human)
}

fn web() -> AppId {
    AppId::new("web", "shop")
}

fn desired() -> DesiredState {
    let mut desired = DesiredState::default();
    desired.apps.insert(web(), spec("web:1.4", 3));
    desired
        .apps
        .insert(AppId::new("api", "shop"), spec("api:1", 2));
    desired
}

fn scale(app: &str, replicas: u32) -> ProposalRequest {
    ProposalRequest {
        kind: RequestKind::Scale,
        namespace: Some("shop".to_string()),
        app: Some(app.to_string()),
        replicas: Some(replicas),
        config: None,
        of: None,
        fault: None,
        node: None,
        reason: "p99 over the SLO".to_string(),
        ttl_seconds: None,
        idempotency_key: None,
    }
}

/// A world: desired state with its proposals and leases, driven the way
/// the council's state machine drives it.
struct World {
    desired: DesiredState,
}

impl World {
    fn new() -> Self {
        World { desired: desired() }
    }

    fn propose(
        &mut self,
        request: &ProposalRequest,
        author: &Principal,
        now: u64,
    ) -> ProposalOutcome {
        let proposal = match build(&self.desired, request, author, now, as_declared) {
            Ok(proposal) => proposal,
            Err(refusal) => return ProposalOutcome::Refused { refusal },
        };
        let snapshot = self.desired.clone();
        let desired = &mut self.desired;
        desired.proposals.create(
            &mut desired.agent_leases,
            proposal,
            |app| current_revision(&snapshot, app),
            now,
        )
    }

    fn created(&mut self, request: &ProposalRequest, author: &Principal) -> ProposalId {
        match self.propose(request, author, NOW) {
            ProposalOutcome::Created { proposal } => proposal.id,
            other => panic!("expected a new proposal, got {other:?}"),
        }
    }

    /// Approve and, when the checks pass, write `after` as the state
    /// machine does.
    fn approve(
        &mut self,
        id: ProposalId,
        approval: Approval,
        now: u64,
    ) -> Result<Proposal, ApplyBlock> {
        let snapshot = self.desired.clone();
        let targets: Vec<(AppId, AppRevision)> = self
            .desired
            .proposals
            .check_apply(
                id,
                &approval,
                &self.desired.agent_leases,
                |app| current_revision(&snapshot, app),
                now,
            )
            .map(|proposal| {
                proposal
                    .changes
                    .iter()
                    .map(|change| (change.app.clone(), change.after.clone()))
                    .collect()
            })?;
        for (app, target) in targets {
            write(&mut self.desired, &app, &target);
        }
        let desired = &mut self.desired;
        Ok(desired
            .proposals
            .mark_applied(&mut desired.agent_leases, id, approval, 7, now)
            .unwrap())
    }

    fn fail_or_refuse(&mut self, id: ProposalId, approval: Approval, now: u64) -> ApplyBlock {
        match self.approve(id, approval.clone(), now) {
            Ok(proposal) => panic!("expected a refusal, got {:?}", proposal.state),
            Err(ApplyBlock::Fail(why)) => {
                let desired = &mut self.desired;
                desired.proposals.mark_failed(
                    &mut desired.agent_leases,
                    id,
                    approval,
                    why.clone(),
                    now,
                );
                ApplyBlock::Fail(why)
            }
            Err(refuse) => refuse,
        }
    }

    fn undo(&mut self, id: ProposalId, by: &Principal, now: u64) -> Result<(), ProposalRefusal> {
        let snapshot = self.desired.clone();
        let targets: Vec<(AppId, AppRevision)> = self
            .desired
            .proposals
            .check_undo(
                id,
                by,
                &self.desired.agent_leases,
                |app| current_revision(&snapshot, app),
                now,
            )
            .map(|proposal| {
                proposal
                    .changes
                    .iter()
                    .map(|change| (change.app.clone(), change.after_undo()))
                    .collect()
            })?;
        for (app, target) in targets {
            write(&mut self.desired, &app, &target);
        }
        self.desired
            .proposals
            .mark_undone(id, Approval::Human(by.clone()), now, None);
        Ok(())
    }

    fn state(&self, id: ProposalId) -> &'static str {
        self.desired.proposals.get(id).unwrap().state.name()
    }
}

/// What the council's state machine does with a target revision, minus
/// its other bookkeeping.
fn write(desired: &mut DesiredState, app: &AppId, target: &AppRevision) {
    match target.spec.as_deref() {
        None => {
            desired.apps.remove(app);
            desired.stopped_apps.remove(app);
        }
        Some(spec) => {
            desired.apps.insert(app.clone(), spec.clone());
            if target.stopped {
                desired.stopped_apps.insert(app.clone());
            } else {
                desired.stopped_apps.remove(app);
            }
        }
    }
}

fn human(principal: Principal) -> Approval {
    Approval::Human(principal)
}

#[test]
fn a_proposal_is_numbered_pending_and_leases_its_apps() {
    let mut world = World::new();
    let id = world.created(&scale("web", 5), &holmes());
    assert_eq!(id, ProposalId(1));
    let proposal = world.desired.proposals.get(id).unwrap();
    assert_eq!(proposal.state, ProposalState::Pending);
    assert_eq!(proposal.undo.command.as_deref(), Some("relish undo 1"));
    assert_eq!(
        proposal.summary.last().unwrap(),
        "Undo: `relish undo 1` puts web back to 3 replicas."
    );
    let lease = &world.desired.agent_leases.leases["app:shop/web"];
    assert_eq!(lease.holder.name, "holmes");
    assert_eq!(lease.proposal, Some(1));
    assert_eq!(lease.expires_at_unix_ms, proposal.expires_at_unix_ms);
}

#[test]
fn approving_applies_after_and_ends_the_lease() {
    let mut world = World::new();
    let id = world.created(&scale("web", 5), &holmes());
    let applied = world.approve(id, human(alice()), NOW + 10).unwrap();
    assert!(matches!(
        applied.state,
        ProposalState::Applied { log_index: 7, .. }
    ));
    assert_eq!(
        world.desired.apps[&web()].replicas,
        crate::config::Replicas::Fixed(5)
    );
    assert!(world.desired.agent_leases.leases.is_empty());
}

#[test]
fn a_policy_may_approve() {
    let mut world = World::new();
    let id = world.created(&scale("web", 5), &holmes());
    let applied = world
        .approve(
            id,
            Approval::Policy {
                name: "agents-scale-shop".to_string(),
            },
            NOW,
        )
        .unwrap();
    assert_eq!(
        applied.view(NOW).approval.approved_by.as_deref(),
        Some("policy:agents-scale-shop")
    );
}

#[test]
fn nobody_approves_their_own_proposal_and_no_agent_approves() {
    let mut world = World::new();
    let id = world.created(&scale("web", 5), &alice());
    // Alice's token rotated: a new credential under the same name.
    let mut rotated = alice();
    rotated.principal_id = "token:alice-rotated".to_string();
    assert!(matches!(
        world.fail_or_refuse(id, human(rotated), NOW),
        ApplyBlock::Refuse(ProposalRefusal::SelfApproval { .. })
    ));
    let agent = principal("capacity", TokenKind::Agent);
    assert!(matches!(
        world.fail_or_refuse(id, human(agent.clone()), NOW),
        ApplyBlock::Refuse(ProposalRefusal::AgentDecides { .. })
    ));
    let desired = &mut world.desired;
    assert!(matches!(
        desired
            .proposals
            .reject(&mut desired.agent_leases, id, &agent, "no", NOW),
        ProposalOutcome::Refused {
            refusal: ProposalRefusal::AgentDecides { .. }
        }
    ));
    assert_eq!(world.state(id), "pending");
}

#[test]
fn a_decided_proposal_cant_be_decided_again() {
    let mut world = World::new();
    let bob = principal("bob", TokenKind::Human);
    let rejected = world.created(&scale("web", 5), &holmes());
    let desired = &mut world.desired;
    assert!(matches!(
        desired.proposals.reject(
            &mut desired.agent_leases,
            rejected,
            &alice(),
            "not now",
            NOW
        ),
        ProposalOutcome::Rejected { .. }
    ));
    assert!(matches!(
        world.fail_or_refuse(rejected, human(bob.clone()), NOW),
        ApplyBlock::Refuse(ProposalRefusal::WrongState { .. })
    ));
    let applied = world.created(&scale("web", 6), &holmes());
    world.approve(applied, human(alice()), NOW).unwrap();
    let desired = &mut world.desired;
    // Rejecting or withdrawing an applied proposal is refused.
    assert!(matches!(
        desired
            .proposals
            .reject(&mut desired.agent_leases, applied, &bob, "late", NOW),
        ProposalOutcome::Refused {
            refusal: ProposalRefusal::WrongState { .. }
        }
    ));
    assert!(matches!(
        desired
            .proposals
            .withdraw(&mut desired.agent_leases, applied, &holmes(), NOW),
        ProposalOutcome::Refused {
            refusal: ProposalRefusal::WrongState { .. }
        }
    ));
    assert!(matches!(
        world.fail_or_refuse(applied, human(bob), NOW),
        ApplyBlock::Refuse(ProposalRefusal::WrongState { .. })
    ));
}

#[test]
fn only_the_author_withdraws() {
    let mut world = World::new();
    let id = world.created(&scale("web", 5), &holmes());
    let desired = &mut world.desired;
    assert!(matches!(
        desired
            .proposals
            .withdraw(&mut desired.agent_leases, id, &alice(), NOW),
        ProposalOutcome::Refused {
            refusal: ProposalRefusal::NotAuthor { .. }
        }
    ));
    assert!(matches!(
        desired
            .proposals
            .withdraw(&mut desired.agent_leases, id, &holmes(), NOW),
        ProposalOutcome::Withdrawn { .. }
    ));
    assert!(world.desired.agent_leases.leases.is_empty());
}

#[test]
fn an_app_that_moved_fails_the_proposal_as_stale_and_writes_nothing() {
    let mut world = World::new();
    let id = world.created(&scale("web", 5), &holmes());
    // A person applies a new image in between.
    world.desired.apps.insert(web(), spec("web:1.5", 3));
    let block = world.fail_or_refuse(id, human(alice()), NOW);
    assert!(
        matches!(block, ApplyBlock::Fail(FailedReason::Stale { .. })),
        "{block:?}"
    );
    assert_eq!(world.state(id), "failed");
    assert_eq!(world.desired.apps[&web()].image.as_deref(), Some("web:1.5"));
    assert_eq!(
        world.desired.apps[&web()].replicas,
        crate::config::Replicas::Fixed(3)
    );
    assert!(world.desired.agent_leases.leases.is_empty());
}

#[test]
fn someone_elses_lease_refuses_a_proposal_and_fails_an_approval() {
    let mut world = World::new();
    let capacity = principal("capacity", TokenKind::Agent);
    world
        .desired
        .agent_leases
        .take(
            &crate::proposal::lease::LeaseRequest {
                target: LeaseTarget::namespace("shop"),
                holder: crate::proposal::lease::LeaseHolder {
                    name: "capacity".to_string(),
                    principal_id: capacity.principal_id.clone(),
                },
                purpose: "rebalancing".to_string(),
                ttl_ms: DEFAULT_LEASE_TTL_MS,
            },
            NOW,
        )
        .unwrap();
    match world.propose(&scale("web", 5), &holmes(), NOW) {
        ProposalOutcome::Refused {
            refusal: ProposalRefusal::LeaseHeld { lease },
        } => assert_eq!(lease.holder.name, "capacity"),
        other => panic!("{other:?}"),
    }
    // Nothing half-taken.
    assert_eq!(world.desired.agent_leases.leases.len(), 1);

    // Once that lease lapses the proposal is made; if someone else then
    // holds a lease on the app, approval fails on it.
    let later = NOW + DEFAULT_LEASE_TTL_MS + 1;
    let id = match world.propose(&scale("web", 5), &holmes(), later) {
        ProposalOutcome::Created { proposal } => proposal.id,
        other => panic!("{other:?}"),
    };
    world
        .desired
        .agent_leases
        .break_lease(&LeaseTarget::app("shop", "web"), later)
        .unwrap();
    world.desired.agent_leases.expire(later);
    world
        .desired
        .agent_leases
        .take(
            &crate::proposal::lease::LeaseRequest {
                target: LeaseTarget::app("shop", "web"),
                holder: crate::proposal::lease::LeaseHolder {
                    name: "alice".to_string(),
                    principal_id: alice().principal_id,
                },
                purpose: "fixing it by hand".to_string(),
                ttl_ms: DEFAULT_LEASE_TTL_MS,
            },
            later,
        )
        .unwrap();
    let bob = principal("bob", TokenKind::Human);
    assert!(matches!(
        world.fail_or_refuse(id, human(bob), later + 1),
        ApplyBlock::Fail(FailedReason::Lease { .. })
    ));
}

#[test]
fn one_pending_proposal_per_app_naming_the_first() {
    let mut world = World::new();
    let first = world.created(&scale("web", 5), &holmes());
    match world.propose(&scale("web", 6), &holmes(), NOW) {
        ProposalOutcome::Refused {
            refusal: ProposalRefusal::Conflict { proposal, app },
        } => {
            assert_eq!(proposal, first);
            assert_eq!(app, "shop/web");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_author_may_have_eight_pending_proposals() {
    let mut world = World::new();
    for index in 0..MAX_PENDING_PER_AUTHOR {
        let name = format!("app{index}");
        world
            .desired
            .apps
            .insert(AppId::new(&name, "shop"), spec("x:1", 1));
        world.created(&scale(&name, 2), &holmes());
    }
    world
        .desired
        .apps
        .insert(AppId::new("one-more", "shop"), spec("x:1", 1));
    assert!(matches!(
        world.propose(&scale("one-more", 2), &holmes(), NOW),
        ProposalOutcome::Refused {
            refusal: ProposalRefusal::Limit { .. }
        }
    ));
}

#[test]
fn a_retry_with_the_same_key_returns_the_proposal_already_made() {
    let mut world = World::new();
    let mut request = scale("web", 5);
    request.idempotency_key = Some("holmes-7c1e".to_string());
    let id = world.created(&request, &holmes());
    match world.propose(&request, &holmes(), NOW + 5) {
        ProposalOutcome::Existing { proposal } => assert_eq!(proposal.id, id),
        other => panic!("{other:?}"),
    }
    assert_eq!(world.desired.proposals.last_id, 1);
}

/// A token name can be reused once its token is revoked. The new token is
/// another credential, so it isn't the author of the old one's proposals:
/// it can't withdraw them or answer with them for an idempotency key.
#[test]
fn a_new_token_under_a_reused_name_isnt_the_author() {
    let mut world = World::new();
    let mut request = scale("web", 5);
    request.idempotency_key = Some("holmes-7c1e".to_string());
    let id = world.created(&request, &holmes());
    let mut reissued = holmes();
    reissued.principal_id = "token:holmes-reissued".to_string();

    let mut elsewhere = scale("api", 4);
    elsewhere.idempotency_key = request.idempotency_key.clone();
    match world.propose(&elsewhere, &reissued, NOW + 5) {
        ProposalOutcome::Created { proposal } => assert_ne!(proposal.id, id),
        other => panic!("the old proposal answered another credential's key: {other:?}"),
    }
    let desired = &mut world.desired;
    assert!(matches!(
        desired
            .proposals
            .withdraw(&mut desired.agent_leases, id, &reissued, NOW),
        ProposalOutcome::Refused {
            refusal: ProposalRefusal::NotAuthor { .. }
        }
    ));
    assert_eq!(world.state(id), "pending");
}

/// The per-author limit counts a rotated token's proposals as its own:
/// rotating doesn't buy eight more.
#[test]
fn a_rotated_token_shares_its_authors_limit() {
    let mut world = World::new();
    for index in 0..MAX_PENDING_PER_AUTHOR {
        let name = format!("app{index}");
        world
            .desired
            .apps
            .insert(AppId::new(&name, "shop"), spec("x:1", 1));
        world.created(&scale(&name, 2), &holmes());
    }
    world
        .desired
        .apps
        .insert(AppId::new("one-more", "shop"), spec("x:1", 1));
    let mut rotated = holmes();
    rotated.principal_id = "token:holmes-rotated".to_string();
    assert!(matches!(
        world.propose(&scale("one-more", 2), &rotated, NOW),
        ProposalOutcome::Refused {
            refusal: ProposalRefusal::Limit { .. }
        }
    ));
}

#[test]
fn a_proposal_past_its_ttl_expires_and_cant_be_approved() {
    let mut world = World::new();
    let id = world.created(&scale("web", 5), &holmes());
    let expires = world.desired.proposals.get(id).unwrap().expires_at_unix_ms;
    assert!(matches!(
        world.fail_or_refuse(id, human(alice()), expires),
        ApplyBlock::Refuse(ProposalRefusal::WrongState { .. })
    ));
    assert!(world.desired.proposals.needs_reaping(expires));
    let desired = &mut world.desired;
    assert!(
        desired
            .proposals
            .expire(&mut desired.agent_leases, expires - 1)
            .is_empty()
    );
    let expired = desired.proposals.expire(&mut desired.agent_leases, expires);
    assert_eq!(expired.len(), 1);
    assert_eq!(world.state(id), "expired");
    assert!(world.desired.agent_leases.leases.is_empty());
}

#[test]
fn undo_restores_the_exact_before_revision() {
    let mut world = World::new();
    let before = current_revision(&world.desired, &web());
    let id = world.created(&scale("web", 5), &holmes());
    world.approve(id, human(alice()), NOW).unwrap();
    assert_ne!(current_revision(&world.desired, &web()), before);
    world.undo(id, &alice(), NOW + 60_000).unwrap();
    assert_eq!(current_revision(&world.desired, &web()), before);
    assert_eq!(world.state(id), "undone");
    // An undone proposal can't be undone again.
    assert!(matches!(
        world.undo(id, &alice(), NOW + 61_000),
        Err(ProposalRefusal::WrongState { .. })
    ));
}

#[test]
fn undo_refuses_a_pending_proposal_an_agent_and_an_app_that_moved() {
    let mut world = World::new();
    let id = world.created(&scale("web", 5), &holmes());
    assert!(matches!(
        world.undo(id, &alice(), NOW),
        Err(ProposalRefusal::WrongState { .. })
    ));
    world.approve(id, human(alice()), NOW).unwrap();
    assert!(matches!(
        world.undo(id, &holmes(), NOW),
        Err(ProposalRefusal::AgentDecides { .. })
    ));
    world.desired.apps.insert(web(), spec("web:1.6", 5));
    match world.undo(id, &alice(), NOW) {
        Err(ProposalRefusal::Stale { app, what }) => {
            assert_eq!(app, "shop/web");
            assert!(what.contains("after proposal 1 applied"), "{what}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(world.desired.apps[&web()].image.as_deref(), Some("web:1.6"));
}

#[test]
fn undo_is_refused_past_the_window_and_after_compaction() {
    let mut world = World::new();
    let id = world.created(&scale("web", 5), &holmes());
    world.approve(id, human(alice()), NOW).unwrap();
    let late = NOW + UNDO_WINDOW_MS;
    match world.undo(id, &alice(), late) {
        Err(ProposalRefusal::NoUndo { reason }) => {
            assert!(
                reason.contains("relish rollback web --namespace shop"),
                "{reason}"
            );
        }
        other => panic!("{other:?}"),
    }
    let (compacted, removed) = world.desired.proposals.compact(late);
    assert_eq!(compacted, vec![id]);
    assert!(removed.is_empty());
    let record = world.desired.proposals.get(id).unwrap();
    assert!(record.compacted);
    assert!(record.changes[0].before.spec.is_none());
    assert!(!record.summary.is_empty());
    assert!(matches!(
        world.undo(id, &alice(), NOW + 1),
        Err(ProposalRefusal::NoUndo { .. })
    ));
}

#[test]
fn compaction_keeps_pending_and_undoable_proposals_whole() {
    let mut world = World::new();
    let pending = world.created(&scale("web", 5), &holmes());
    let applied = world.created(&scale("api", 3), &holmes());
    world.approve(applied, human(alice()), NOW).unwrap();
    let (compacted, _) = world.desired.proposals.compact(NOW + 1);
    assert!(compacted.is_empty());
    assert!(!world.desired.proposals.needs_reaping(NOW + 1));
    assert!(
        world.desired.proposals.get(pending).unwrap().changes[0]
            .after
            .spec
            .is_some()
    );
}

#[test]
fn an_agents_undo_is_a_proposal_that_marks_the_original_undone() {
    let mut world = World::new();
    let before = current_revision(&world.desired, &web());
    let original = world.created(&scale("web", 5), &holmes());
    world.approve(original, human(alice()), NOW).unwrap();
    let undo = ProposalRequest {
        kind: RequestKind::Undo,
        of: Some(original.0),
        fault: None,
        node: None,
        namespace: None,
        app: None,
        replicas: None,
        config: None,
        reason: "made it worse".to_string(),
        ttl_seconds: None,
        idempotency_key: None,
    };
    let id = world.created(&undo, &holmes());
    let proposal = world.desired.proposals.get(id).unwrap();
    assert_eq!(proposal.kind, ProposalKind::Undo { of: original });
    assert_eq!(proposal.reversibility, Reversibility::Exact);
    world.approve(id, human(alice()), NOW + 1).unwrap();
    assert_eq!(current_revision(&world.desired, &web()), before);
    match &world.desired.proposals.get(original).unwrap().state {
        ProposalState::Undone {
            undo_proposal,
            undone_by,
            ..
        } => {
            assert_eq!(*undo_proposal, Some(id));
            assert_eq!(undone_by.label(), "alice");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_stop_then_its_undo_starts_the_app_with_the_same_spec() {
    let mut world = World::new();
    let before = current_revision(&world.desired, &web());
    let mut stop = scale("web", 1);
    stop.kind = RequestKind::Stop;
    stop.replicas = None;
    let id = world.created(&stop, &holmes());
    world.approve(id, human(alice()), NOW).unwrap();
    assert!(world.desired.stopped_apps.contains(&web()));
    world.undo(id, &alice(), NOW).unwrap();
    assert!(!world.desired.stopped_apps.contains(&web()));
    assert_eq!(current_revision(&world.desired, &web()), before);
}
