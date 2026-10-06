//! A partition from one app answers only once it reaches every instance of
//! the app (#625).
//!
//! The partition is keyed by each caller's cgroup, which the runtime names
//! only when asked, and the injecting turn asks under its 500 ms budget. Here
//! each read takes 300 ms, as runc's owner did on a loaded runner: the first
//! of three frontends fits the turn and the other two don't.

use super::loop_harness::replicated;
use super::*;
use crate::bun::agent::fault_coverage::Injection;
use crate::smoker::network::LateCuts;
use crate::smoker::types::{FaultRequest, FaultSummary, FaultType};

const SLOW_CGROUP_READ: std::time::Duration = std::time::Duration::from_millis(300);

fn partition_from_frontend() -> FaultRequest {
    FaultRequest {
        fault_type: FaultType::Partition {
            source_app: Some("frontend".into()),
        },
        target_service: "redis".into(),
        namespace: Some("default".into()),
        target_instance: None,
        target_node: None,
        duration: std::time::Duration::from_secs(60),
        injected_by: "test".into(),
        reason: None,
        include_leader: false,
        override_safety: false,
        acknowledged: true,
    }
}

/// Three frontends, a partition from them in the registry, and a turn that
/// has just read their cgroups slowly, as `InjectFault`'s does.
async fn partition_after_a_slow_turn() -> (TestAgent, MockGrill, crate::smoker::types::FaultRule) {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("frontend", 3)).await);
    let rule = agent.fault_registry.insert(&partition_from_frontend());
    grill.set_workload_cgroup_delay(Some(SLOW_CGROUP_READ));
    agent.turn_deadline = Some(tokio::time::Instant::now() + TURN_RUNTIME_BUDGET);
    agent.local_callers().await;
    agent.turn_deadline = None;
    (agent, grill, rule)
}

fn pending_ids(agent: &TestAgent) -> Vec<String> {
    let mut ids: Vec<String> = agent
        .network_faults
        .pending_callers
        .iter()
        .map(|caller| caller.id.0.clone())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn a_caller_whose_cgroup_read_outlasts_the_turn_is_pending() {
    let (agent, _grill, _rule) = partition_after_a_slow_turn().await;
    // The supervisor lists instances in no fixed order, so which frontend
    // fits the turn varies; two of the three never do.
    let pending = pending_ids(&agent);
    assert_eq!(pending.len(), 2, "{pending:?}");
    assert!(
        pending
            .iter()
            .all(|id| id.starts_with("default__frontend-")),
        "{pending:?}"
    );
}

#[tokio::test]
async fn an_injection_answers_only_after_its_unnamed_callers_are_read() {
    let (mut agent, _grill, rule) = partition_after_a_slow_turn().await;
    let (response, mut answer) = oneshot::channel();
    agent
        .answer_fault_injection(Injection::new(
            rule.id,
            FaultSummary::from(&rule),
            LateCuts::default(),
            response,
        ))
        .await;
    assert!(
        answer.try_recv().is_err(),
        "the partition was reported in place before two frontends were read"
    );

    let read = agent.follow_ups.join_next_with_id().await.unwrap();
    agent.apply_follow_up(read).await;
    let summary = answer.await.unwrap().expect("every frontend was read");
    assert_eq!(summary.id, rule.id.0);
    assert!(agent.follow_ups.is_empty());
}

#[tokio::test]
async fn an_injection_cleared_while_its_callers_are_read_is_refused() {
    let (mut agent, _grill, rule) = partition_after_a_slow_turn().await;
    let (response, answer) = oneshot::channel();
    agent
        .answer_fault_injection(Injection::new(
            rule.id,
            FaultSummary::from(&rule),
            LateCuts::default(),
            response,
        ))
        .await;
    agent.fault_registry.remove(rule.id);

    let read = agent.follow_ups.join_next_with_id().await.unwrap();
    agent.apply_follow_up(read).await;
    let error = answer.await.unwrap().unwrap_err().to_string();
    assert!(error.contains("cleared"), "{error}");
}

#[tokio::test]
async fn an_injection_with_every_caller_named_answers_in_its_turn() {
    let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("frontend", 3)).await);
    let rule = agent.fault_registry.insert(&partition_from_frontend());
    agent.turn_deadline = Some(tokio::time::Instant::now() + TURN_RUNTIME_BUDGET);
    agent.local_callers().await;
    let (response, mut answer) = oneshot::channel();
    agent
        .answer_fault_injection(Injection::new(
            rule.id,
            FaultSummary::from(&rule),
            LateCuts::default(),
            response,
        ))
        .await;
    assert!(answer.try_recv().unwrap().is_ok());
    assert!(agent.follow_ups.is_empty());
}
