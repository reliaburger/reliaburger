//! Status from the snapshot the loop publishes (#351, stage 2).
//!
//! A [`StatusReader`] answers without queueing for the loop, so it must say
//! how old its answer is, refuse one that is too old, and never report an
//! exited container as running just because the loop hasn't noticed yet.

use super::loop_harness::{crash, replicated};
use super::*;
use crate::grill::mock::MockCall;
use status_snapshot::{STATUS_FRESHNESS_WAIT, STATUS_SNAPSHOT_MAX_AGE};

/// While the loop is in a long turn, the reader still answers at once, from
/// the snapshot the loop published before it.
#[tokio::test]
async fn status_answers_from_the_snapshot_while_the_loop_is_busy() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let reader = agent.status_reader();
    let task = tokio::spawn(async move { agent.run().await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // `Logs` still reads its capture inline (stage 3), so it holds a turn.
    grill.set_call_delay(MockCall::Logs, Some(std::time::Duration::from_millis(1500)));
    let (response, _logs) = oneshot::channel();
    tx.send(AgentCommand::Logs {
        app_name: "web".into(),
        namespace: "default".into(),
        tail: None,
        response,
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let asked = std::time::Instant::now();
    let statuses = reader.read().await.unwrap();
    assert!(
        asked.elapsed() < std::time::Duration::from_millis(500),
        "status waited {:?} for a busy loop",
        asked.elapsed()
    );
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].state, "running");
    let age = statuses[0]
        .status_age_ms
        .expect("the answer didn't carry its age");
    assert!(
        age >= 400 && age <= STATUS_SNAPSHOT_MAX_AGE.as_millis() as u64,
        "the answer's age ({age} ms) doesn't match the busy turn"
    );
    stop_agent_task(shutdown, task).await;
}

/// A loop that stopped publishing (stuck, or never started) gets no answer
/// from its old snapshot: after waiting for a fresh one, the reader fails,
/// as a status command that timed out used to.
#[tokio::test(start_paused = true)]
async fn status_refuses_a_snapshot_older_than_its_bound() {
    let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let reader = agent.status_reader();
    agent.publish_status();
    tokio::time::advance(STATUS_SNAPSHOT_MAX_AGE + std::time::Duration::from_millis(1)).await;

    let asked = tokio::time::Instant::now();
    let answer = reader.read().await;
    assert!(
        matches!(answer, Err(StatusUnavailable::Stale { .. })),
        "a stale snapshot answered: {answer:?}"
    );
    assert!(asked.elapsed() >= STATUS_FRESHNESS_WAIT);
}

/// A fresh publication while the reader waits is used at once.
#[tokio::test(start_paused = true)]
async fn status_waits_for_the_next_publication_when_its_snapshot_is_old() {
    let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let reader = agent.status_reader();
    agent.publish_status();
    tokio::time::advance(STATUS_SNAPSHOT_MAX_AGE * 2).await;

    let read = reader.read();
    tokio::pin!(read);
    // Poll once so the reader starts waiting, then publish.
    assert!(futures_util::poll!(read.as_mut()).is_pending());
    agent.publish_status();
    let statuses = read.await.unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].status_age_ms, Some(0));
}

/// Once the agent is gone, nothing will publish again.
#[tokio::test(start_paused = true)]
async fn status_reports_a_stopped_agent_once_its_snapshot_is_old() {
    let (agent, _tx, _shutdown, _grill) = test_agent_with_grill();
    let reader = agent.status_reader();
    drop(agent);
    tokio::time::advance(STATUS_SNAPSHOT_MAX_AGE * 2).await;
    assert!(matches!(
        reader.read().await,
        Err(StatusUnavailable::AgentStopped)
    ));
}

/// The loop last saw the replica running and hasn't ticked since it died.
/// Status believes the runtime: the replica is reported stopped.
#[tokio::test]
async fn status_reports_an_exited_instance_before_the_loop_notices() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let reader = agent.status_reader();
    agent.publish_status();
    crash(&grill);

    let statuses = reader.read().await.unwrap();
    assert_eq!(statuses[0].state, "stopped");
    assert_eq!(statuses[0].exit_code, Some(1));
    assert_eq!(
        agent
            .supervisor
            .get_instance(&InstanceId("default__web-0".into()))
            .unwrap()
            .state,
        ContainerState::Running,
        "the loop's own view shouldn't have changed"
    );
}

/// The `Status` command still works for callers that hold only the command
/// channel, but the loop no longer waits for the runtime to answer it.
#[tokio::test]
async fn the_status_command_reads_the_runtime_off_the_loop() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    grill.set_pid_delay(Some(std::time::Duration::from_secs(30)));
    let (response, answer) = oneshot::channel();
    let started = std::time::Instant::now();
    agent
        .handle_command(AgentCommand::Status { response })
        .await;
    assert!(
        started.elapsed() < std::time::Duration::from_millis(100),
        "the status turn waited {:?} for the runtime",
        started.elapsed()
    );
    let statuses = answer.await.unwrap();
    assert!(statuses[0].runtime_unknown);
}

/// Before its loop first publishes, the agent is still adopting what it
/// found on disk. Status must wait for that, not answer "nothing here".
#[tokio::test(start_paused = true)]
async fn status_waits_for_the_loop_to_publish_for_the_first_time() {
    let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let reader = agent.status_reader();

    let read = reader.read();
    tokio::pin!(read);
    assert!(futures_util::poll!(read.as_mut()).is_pending());
    agent.publish_status();
    assert_eq!(read.await.unwrap().len(), 1);

    let (fresh, _fresh_tx, _fresh_shutdown, _fresh_grill) = test_agent_with_grill();
    assert!(matches!(
        fresh.status_reader().read().await,
        Err(StatusUnavailable::Stale { .. })
    ));
}

/// The liveness check runs beside the pid and exit-code reads, so it
/// doesn't use up their share of the status deadline.
#[tokio::test]
async fn the_liveness_check_does_not_delay_the_pid_read() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    grill.set_pid(4242);
    grill.set_call_delay(MockCall::State, Some(std::time::Duration::from_millis(300)));
    grill.set_pid_delay(Some(std::time::Duration::from_millis(300)));
    let statuses = agent.get_status().await;
    assert!(!statuses[0].runtime_unknown, "{statuses:?}");
    assert_eq!(statuses[0].pid, Some(4242));
}
