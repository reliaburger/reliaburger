//! Job retries must stop after success or an explicit operator stop.

use crate::{
    bun::agent::{AgentCommand, ApplyEvent, BunAgent, InstanceStatus},
    config::Config,
    grill::{ContainerState, InstanceId, mock::MockGrill, port::PortAllocator},
};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

async fn status(tx: &mpsc::Sender<AgentCommand>) -> InstanceStatus {
    let (response, result) = oneshot::channel();
    tx.send(AgentCommand::Status { response }).await.unwrap();
    result.await.unwrap().into_iter().next().unwrap()
}

async fn completed_retry(explicit_stop: bool) -> InstanceStatus {
    let (tx, rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let grill = MockGrill::new();
    let mut agent = BunAgent::new(
        grill.clone(),
        PortAllocator::new(30000, 31000),
        rx,
        shutdown.clone(),
    );
    let task = tokio::spawn(async move { agent.run().await });
    let (events, mut results) = mpsc::channel(32);
    tx.send(AgentCommand::Deploy {
        config: Config::parse("[job.work]\nimage = 'test:v1'\n").unwrap(),
        events,
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match results.recv().await.unwrap() {
                ApplyEvent::Complete { .. } => break,
                ApplyEvent::Error { message } => panic!("{message}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let instance = status(&tx).await;
    let id = InstanceId(instance.id);
    grill.set_exit_code(&id, Some(1));
    grill.set_state(&id, ContainerState::Stopped);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let instance = status(&tx).await;
            if instance.state == "running" && instance.restart_count == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the first failed job must retry");
    if explicit_stop {
        let (response, result) = oneshot::channel();
        tx.send(AgentCommand::Stop {
            app_name: "work".into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
        result.await.unwrap().unwrap();
    } else {
        grill.set_exit_code(&id, Some(0));
        grill.set_state(&id, ContainerState::Stopped);
    }
    // Cross the real exponential backoff (std::time::Instant) and several
    // agent ticks so a spurious retry cannot hide between observations.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let final_status = status(&tx).await;
    shutdown.cancel();
    task.await.unwrap();
    final_status
}

#[tokio::test]
async fn successful_retry_stays_completed() {
    let instance = completed_retry(false).await;
    assert_eq!(instance.state, "stopped");
    assert_eq!(instance.restart_count, 1);
    assert_eq!(instance.exit_code, Some(0));
}

#[tokio::test]
async fn explicitly_stopped_retry_does_not_run_again() {
    let instance = completed_retry(true).await;
    assert_eq!(instance.state, "stopped");
    assert_eq!(instance.restart_count, 1);
}
