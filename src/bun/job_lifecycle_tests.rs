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

#[tokio::test]
async fn cron_worker_owns_its_target_until_runtime_creation_finishes() {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let (tx, rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let grill = MockGrill::new();
    grill.block_creates();
    let mut agent = BunAgent::new(
        grill.clone(),
        PortAllocator::new(30000, 31000),
        rx,
        shutdown.clone(),
    );
    let task = tokio::spawn(async move { agent.run().await });
    let (events, mut results) = mpsc::channel(32);
    tx.send(AgentCommand::Deploy {
        config: Config::parse("[job.cron]\nimage = 'test:v1'\nschedule = '* * * * *'\n").unwrap(),
        events,
    })
    .await
    .unwrap();
    while let Some(event) = results.recv().await {
        if matches!(event, ApplyEvent::Complete { .. }) {
            break;
        }
        assert!(!matches!(event, ApplyEvent::Error { .. }), "{event:?}");
    }
    tokio::time::timeout(Duration::from_secs(5), grill.wait_for_creates(1))
        .await
        .unwrap();
    let (response, result) = oneshot::channel();
    tx.send(AgentCommand::DeployOperations { response })
        .await
        .unwrap();
    let operations = result.await.unwrap();
    let app = crate::bun::api::router(
        tx.clone(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        0,
        None,
    );
    let stopped_during_create = app
        .clone()
        .oneshot(
            Request::post("/v1/stop/cron/default")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let busy_status = stopped_during_create.status();
    let busy_body = to_bytes(stopped_during_create.into_body(), 65536)
        .await
        .unwrap();
    let (events, mut overlap) = mpsc::channel(8);
    tx.send(AgentCommand::Deploy {
        config: Config::parse("[job.cron]\nimage = 'test:v2'\n").unwrap(),
        events,
    })
    .await
    .unwrap();
    let overlapping_result = tokio::time::timeout(Duration::from_secs(2), overlap.recv())
        .await
        .unwrap()
        .unwrap();
    grill.release_creates(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (response, result) = oneshot::channel();
            tx.send(AgentCommand::DeployOperations { response })
                .await
                .unwrap();
            if result.await.unwrap().active_deploys.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let stopped_after_create = app
        .oneshot(
            Request::post("/v1/stop/cron/default")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    shutdown.cancel();
    task.await.unwrap();
    assert_eq!(
        operations.active_deploys.len(),
        1,
        "cron work must be tracked"
    );
    let operation = &operations.active_deploys[0];
    assert!(operation.targets.iter().any(|target| target.name == "cron"));
    assert_eq!(busy_status, StatusCode::CONFLICT);
    assert!(String::from_utf8_lossy(&busy_body).contains(operation.id.as_str()));
    assert!(
        matches!(overlapping_result, ApplyEvent::Error { .. }),
        "{overlapping_result:?}"
    );
    assert_eq!(stopped_after_create, StatusCode::OK);
}
