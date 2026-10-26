//! A standalone job write that reaches the disk after its caller gave up
//! (#1059). The real blocking writer is held after it has built its
//! checkpoint and before it publishes; the caller's deadline passes; the
//! node must refuse new admission and must not dispatch anything while the
//! outcome is uncertain. Once the write lands and the node restarts, a
//! retry with the same `Idempotency-Key` finds the one run the late write
//! created, and every task of it runs exactly once.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::body::Body;
use http_body_util::BodyExt;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::router_with_upgrade;
use crate::bun::agent::AgentCommand;
use crate::bun::task_array_leader::TaskArrayService;
use crate::bun::task_array_node::{NodeRunner, TaskArrayNode, TaskArrayNodeConfig};
use crate::bun::task_executor::{AttemptOutcome, FakeRunner};

const TASKS: u64 = 20;
const SYNC_INTERVAL: Duration = Duration::from_millis(10);
/// Short, so the caller expires while the writer is held.
const CALLER_DEADLINE: Duration = Duration::from_millis(200);

/// One standalone node with durable job storage under `data`, its leader
/// loop and a fake executor that counts every attempt in `executed`.
struct StandaloneNode {
    app: axum::Router,
    service: Arc<TaskArrayService>,
    node: Arc<TaskArrayNode>,
    shutdown: CancellationToken,
}

impl StandaloneNode {
    async fn start(data: &Path, executed: Arc<AtomicU64>) -> Self {
        let shutdown = CancellationToken::new();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);
        // No agent: task arrays don't go through it. Holding the receiver
        // keeps the leader loop alive until shutdown.
        let agent_shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = agent_shutdown.cancelled() => return,
                    command = cmd_rx.recv() => match command {
                        Some(AgentCommand::CurrentResources { response }) => {
                            let _ = response.send(Vec::new());
                        }
                        Some(AgentCommand::Status { response }) => {
                            let _ = response.send(Vec::new());
                        }
                        Some(_) => {}
                        None => return,
                    },
                }
            }
        });
        let node = Arc::new(TaskArrayNode::new(
            TaskArrayNodeConfig {
                root: data.join("task-arrays"),
                policy: crate::config::process_workloads::ProcessWorkloadsConfig {
                    mount_isolation: false,
                    allowed_binaries: vec!["/bin/sh".into()],
                    ..Default::default()
                },
                default_concurrency: 8,
                backoff: (Duration::ZERO, Duration::ZERO),
                group_commit: Default::default(),
                audit_dir: None,
            },
            NodeRunner::Fake(FakeRunner::new(Duration::ZERO, move |_| {
                executed.fetch_add(1, Ordering::SeqCst);
                AttemptOutcome::Exited { code: 0 }
            })),
        ));
        let service = Arc::new(
            TaskArrayService::with_timings(
                Some(Arc::clone(&node)),
                SYNC_INTERVAL,
                Duration::from_secs(30),
            )
            .with_write_timeout(CALLER_DEADLINE)
            .with_storage(data)
            .await
            .unwrap(),
        );
        let app = router_with_upgrade(
            cmd_tx,
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
            None,
            0,
            None,
            None,
            None,
            "default".into(),
            Some("local".into()),
            crate::bun::build_runner::BuildSettings::with_timeout(900),
            crate::cluster::ClusterHttp::plaintext(),
            5050,
            "http",
            256 * 1024 * 1024,
            false,
            crate::bun::capabilities::StaticCapabilities::default(),
            crate::bun::readiness::ReadinessTracker::new(),
            None,
            None,
            None,
            Some(Arc::clone(&service)),
        );
        Self {
            app,
            service,
            node,
            shutdown,
        }
    }

    /// Submit the array under `key`; the status and the decoded answer.
    async fn submit(&self, key: &str) -> (u16, serde_json::Value) {
        let body = serde_json::json!({
            "name": "late",
            "template": { "runtime": "process", "exec": "/bin/sh", "command": ["{index}"] },
            "spec": { "count": TASKS, "chunk_size": 5 },
        });
        let request = axum::http::Request::post("/v1/batch/array")
            .header("content-type", "application/json")
            .header("idempotency-key", key)
            .body(Body::from(body.to_string()))
            .unwrap();
        let response =
            tokio::time::timeout(Duration::from_secs(10), self.app.clone().oneshot(request))
                .await
                .expect("admission must answer within a bounded time")
                .unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let answer = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::json!(String::from_utf8_lossy(&bytes)));
        (status, answer)
    }

    async fn status(&self, batch_id: u64) -> serde_json::Value {
        let request = axum::http::Request::get(format!("/v1/batch/{batch_id}"))
            .body(Body::empty())
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }
}

/// The run ids the store at `data` has published, read straight from disk.
fn published_runs(data: &Path) -> Vec<u64> {
    let bytes = std::fs::read(data.join("job-state").join("jobs.json")).unwrap();
    let checkpoint: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    checkpoint["arrays"]["arrays"]
        .as_object()
        .unwrap()
        .keys()
        .map(|id| id.parse().unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_publication_after_the_caller_times_out_keeps_one_durable_run_identity() {
    let data = tempfile::tempdir().unwrap();
    let executed = Arc::new(AtomicU64::new(0));
    let first = StandaloneNode::start(data.path(), Arc::clone(&executed)).await;
    let writer = Arc::new(std::sync::Barrier::new(2));
    first.service.pause_next_publication(Arc::clone(&writer));

    // The writer is held before publication, so the caller's deadline passes.
    let (status, answer) = first.submit("late-request").await;
    assert_eq!(status, 503, "{answer}");
    assert!(
        answer.to_string().contains("fenced"),
        "the caller must learn admission and dispatch are fenced: {answer}"
    );
    assert!(
        published_runs(data.path()).is_empty(),
        "nothing published yet"
    );

    // Uncertain publication refuses admission, for a retry and new work alike.
    for key in ["late-request", "other-request"] {
        let (status, answer) = first.submit(key).await;
        assert_eq!(
            status, 503,
            "{key} was admitted through the fence: {answer}"
        );
    }

    // Release the writer: the earlier transaction now reaches the disk.
    let release = Arc::clone(&writer);
    tokio::task::spawn_blocking(move || release.wait())
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while published_runs(data.path()).is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the held write never published"
        );
        tokio::time::sleep(SYNC_INTERVAL).await;
    }
    let published = published_runs(data.path());
    assert_eq!(published.len(), 1, "one run identity: {published:?}");
    let run = published[0];

    // The run is durable now, but this node can't know that its caller saw
    // it: many leader ticks later, nothing has been dispatched.
    tokio::time::sleep(SYNC_INTERVAL * 30).await;
    assert_eq!(
        executed.load(Ordering::SeqCst),
        0,
        "work ran through the fence"
    );
    assert!(
        !first.node.array_dir(run).exists(),
        "the fenced leader dispatched run {run}"
    );
    let (status, _) = first.submit("late-request").await;
    assert_eq!(status, 503, "still fenced until restart");
    first.shutdown.cancel();
    drop(first);

    // Restart and reconcile: the retry finds the late write's run.
    let reopened = StandaloneNode::start(data.path(), Arc::clone(&executed)).await;
    let (status, answer) = reopened.submit("late-request").await;
    assert_eq!(status, 202, "{answer}");
    assert_eq!(answer["batch_id"], run, "the retry created a second run");
    assert_eq!(published_runs(data.path()), vec![run]);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let summary = loop {
        let summary = reopened.status(run).await;
        if summary["done"] == true {
            break summary;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "run {run} never finished: {summary}"
        );
        tokio::time::sleep(SYNC_INTERVAL).await;
    };
    assert_eq!(summary["status"], "Succeeded", "{summary}");
    assert_eq!(summary["succeeded"], TASKS, "{summary}");
    assert_eq!(summary["not_run"], 0, "{summary}");
    assert_eq!(
        executed.load(Ordering::SeqCst),
        TASKS,
        "every task ran exactly once"
    );

    // New work takes the next identity, never the late run's.
    let (status, answer) = reopened.submit("next-request").await;
    assert_eq!(status, 202, "{answer}");
    assert!(answer["batch_id"].as_u64().unwrap() > run, "{answer}");
    reopened.shutdown.cancel();
}
