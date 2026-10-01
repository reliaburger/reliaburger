//! Who owns an instance while its restart runs off the loop (#351, stage 2).
//!
//! A restart's kill, create and start run in tasks, so other work can now
//! interleave with them. These tests pin the rules that keep that safe: one
//! restart per instance at a time, a stop or retirement wins over a restart
//! in flight (and waits for the step already running before it signals the
//! runtime), and a state read from before a restart never touches the
//! replacement.

use super::loop_harness::{calls_of, crash, replicated};
use super::*;
use crate::grill::mock::MockCall;

const WEB_0: &str = "default__web-0";

/// Runtime calls on `id` from `from` onwards, by operation name.
fn calls_on(grill: &MockGrill, id: &InstanceId, from: usize) -> Vec<String> {
    grill.calls()[from..]
        .iter()
        .filter(|(_, instance)| instance == id)
        .map(|(operation, _)| operation.clone())
        .collect()
}

/// Deploy one `web` replica, run the loop, crash the replica, and wait until
/// its restart is blocked inside `create`.
async fn restart_blocked_in_create() -> (
    mpsc::Sender<AgentCommand>,
    CancellationToken,
    MockGrill,
    tokio::task::JoinHandle<()>,
) {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    grill.block_creates();
    let task = tokio::spawn(async move { agent.run().await });
    crash(&grill);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        grill.wait_for_creates(1),
    )
    .await
    .expect("the crashed replica's restart never reached create");
    (tx, shutdown, grill, task)
}

/// Send a stop-like command and return its pending answer.
async fn send(
    tx: &mpsc::Sender<AgentCommand>,
    command: impl FnOnce(oneshot::Sender<Result<(), BunError>>) -> AgentCommand,
) -> oneshot::Receiver<Result<(), BunError>> {
    let (response, answer) = oneshot::channel();
    tx.send(command(response)).await.unwrap();
    answer
}

/// The core ordering rule: while the restart's create runs, a stop must not
/// signal the runtime (it could miss the container being created), and once
/// the create finishes the restart must not go on to start it.
async fn assert_stop_wins_over_restart(
    command: impl FnOnce(oneshot::Sender<Result<(), BunError>>) -> AgentCommand,
) {
    let (tx, shutdown, grill, task) = restart_blocked_in_create().await;
    let id = InstanceId(WEB_0.into());
    let before_stop = grill.calls().len();
    let mut answer = send(&tx, command).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        calls_on(&grill, &id, before_stop)
            .iter()
            .all(|operation| operation != "stop" && operation != "kill"),
        "the stop signalled the runtime while the restart was still creating"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(1), &mut answer)
            .await
            .is_err(),
        "the stop finished while the restart's create still held the runtime"
    );

    let released = grill.calls().len();
    grill.release_creates(1);
    tokio::time::timeout(std::time::Duration::from_secs(10), answer)
        .await
        .expect("the stop never finished after the create was released")
        .unwrap()
        .unwrap();
    let after = calls_on(&grill, &id, released);
    assert!(
        !after.iter().any(|operation| operation == "start"),
        "the restart started its replacement after the stop: {after:?}"
    );
    assert!(
        after
            .iter()
            .any(|operation| operation == "stop" || operation == "kill"),
        "the stop never signalled the created container: {after:?}"
    );
    stop_agent_task(shutdown, task).await;
}

#[tokio::test]
async fn a_stop_waits_for_a_restart_in_flight_and_then_wins() {
    assert_stop_wins_over_restart(|response| AgentCommand::Stop {
        app_name: "web".into(),
        namespace: "default".into(),
        response,
    })
    .await;
}

#[tokio::test]
async fn a_retirement_waits_for_a_restart_in_flight_and_then_wins() {
    assert_stop_wins_over_restart(|response| AgentCommand::Retire {
        app_name: "web".into(),
        namespace: "default".into(),
        response,
    })
    .await;
}

/// Ticks keep coming while a restart's kill is slow. None of them may start
/// a second restart of the same instance.
#[tokio::test]
async fn a_slow_restart_is_never_started_twice() {
    let (mut agent, _tx, shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    grill.block_kills();
    let kills_before = calls_of(&grill, "kill");
    let task = tokio::spawn(async move { agent.run().await });
    crash(&grill);
    tokio::time::timeout(std::time::Duration::from_secs(10), grill.wait_for_kills(1))
        .await
        .expect("the crashed replica's restart never reached its kill");
    // At least one more tick, inside the 2 s stop-confirmation timeout the
    // test agent gives the kill (past it, the step fails and a retry is
    // legitimate).
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(
        calls_of(&grill, "kill") - kills_before,
        1,
        "a tick started another restart while the first was still killing"
    );
    grill.release_kills(1);
    stop_agent_task(shutdown, task).await;
}

/// A retirement takes the instance back from its restart: the restart's
/// late create result is dropped, and nothing more of it runs.
#[tokio::test]
async fn a_retired_restart_drops_its_late_result() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let id = InstanceId(WEB_0.into());
    crash(&grill);
    agent.check_apps().await;
    grill.block_creates();
    agent.begin_restart_launches().await;
    // The Clear step: let it finish and start the create.
    let clear = agent.restart_steps.join_next_with_id().await.unwrap();
    agent.finish_restart_step(clear).await;
    grill.wait_for_creates(1).await;

    let taken = agent
        .begin_instance_retirement(&id)
        .await
        .unwrap()
        .expect("the retirement didn't take the instance from its restart");
    let settling =
        tokio::spawn(async move { taken.settle(std::time::Duration::from_secs(5)).await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !settling.is_finished(),
        "settling didn't wait for the create in flight"
    );
    grill.release_creates(1);
    settling.await.unwrap().unwrap();

    let before = grill.calls().len();
    agent.settle_restart_steps().await;
    assert!(agent.restarts.is_empty());
    assert!(
        calls_on(&grill, &id, before).is_empty(),
        "the cancelled restart kept going"
    );
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Stopping
    );
}

/// A sweep reads an instance's state, the instance restarts before the
/// result lands, and the result says "exited". It describes the old
/// container, so the replacement must be left alone.
#[tokio::test]
async fn a_state_read_from_before_a_restart_leaves_the_replacement_alone() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let id = InstanceId(WEB_0.into());
    let reads = agent.plan_state_reads(|_| true);
    assert_eq!(reads.len(), 1);
    crash(&grill);
    let sweep = state_sweep::sweep_states(grill.clone(), reads).await;
    assert_eq!(
        sweep.observations[0].1,
        state_sweep::Observed::Exited { exit_code: None }
    );

    // Meanwhile the instance went through a restart and is Running again.
    let instance = agent.supervisor.get_instance_mut(&id).unwrap();
    instance.restart_count += 1;
    let restarts_before = instance.restart_count;
    agent.apply_state_sweep(Ok(sweep)).await;

    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_eq!(instance.state, ContainerState::Running);
    assert_eq!(instance.restart_count, restarts_before);
    assert!(!instance.retry_pending);
}

/// A job has exited but the runtime couldn't read its exit code (#389).
/// The sweep says it doesn't know, so the loop asks again next tick rather
/// than settling the job as exited without a code.
#[tokio::test]
async fn a_job_exit_code_the_runtime_could_not_read_sweeps_as_unknown() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let job = Config::parse("[job.batch]\nimage = 'batch:v1'\ncommand = ['true']\n").unwrap();
    expect_complete(&drain_deploy(&mut agent, job).await);
    let reads = agent.plan_state_reads(|_| true);
    assert_eq!(reads.len(), 1);
    assert!(reads[0].is_job);
    let id = reads[0].id.clone();
    grill.set_state(&id, ContainerState::Stopped);
    grill.set_exit_code(&id, Some(0));
    grill.set_instance_exit_code_failure(&id, true);
    let sweep = state_sweep::sweep_states(grill.clone(), reads.clone()).await;
    assert_eq!(sweep.observations[0].1, state_sweep::Observed::Unknown);

    grill.set_instance_exit_code_failure(&id, false);
    let sweep = state_sweep::sweep_states(grill.clone(), reads).await;
    assert_eq!(
        sweep.observations[0].1,
        state_sweep::Observed::Exited { exit_code: Some(0) }
    );
}

/// The sweep reads in parallel, under one deadline: ten slow reads take
/// about two batches, not ten reads one after another, and a read that
/// hangs past the deadline comes back unknown instead of holding the sweep.
#[tokio::test]
async fn a_state_sweep_reads_in_parallel_under_one_deadline() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 10)).await);
    let reads = agent.plan_state_reads(|_| true);
    assert_eq!(reads.len(), 10);

    grill.set_call_delay(MockCall::State, Some(std::time::Duration::from_millis(200)));
    let started = std::time::Instant::now();
    let sweep = state_sweep::sweep_states(grill.clone(), reads.clone()).await;
    assert!(started.elapsed() < std::time::Duration::from_millis(900));
    assert!(
        sweep
            .observations
            .iter()
            .all(|(_, observed)| *observed == state_sweep::Observed::Alive)
    );

    grill.set_call_delay(MockCall::State, Some(std::time::Duration::from_secs(30)));
    let started = std::time::Instant::now();
    let sweep = state_sweep::sweep_states(grill.clone(), reads).await;
    assert!(started.elapsed() < state_sweep::STATE_SWEEP_DEADLINE * 2);
    assert!(
        sweep
            .observations
            .iter()
            .all(|(_, observed)| *observed == state_sweep::Observed::Unknown)
    );
}
