//! Integration tests for batch job submission (Phase 12, F1; made
//! durable in 12b.2).
//!
//! A real agent + API router on an ephemeral port (the TestHarness
//! pattern), driving `/v1/batch` end to end: submit → schedule →
//! local dispatch → job completion → tracker summary. The durable
//! tests add a single-node council so batch records live in the Raft
//! state machine: reports are transition-validated, lost callbacks are
//! recovered by the leader's pull watcher, and a "restarted" leader (a
//! fresh API process over the same council) resumes in-flight batches.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use reliaburger::bun::agent::BunAgent;
use reliaburger::bun::api;
use reliaburger::bun::api::NodeMembershipInfo;
use reliaburger::config::Config;
use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo};
use reliaburger::grill::{PortAllocator, ProcessGrill};
use reliaburger::meat::batch_tracker::{BatchJobRecord, BatchRecord, JobStatus, epoch_now_secs};
use reliaburger::relish::client::BunClient;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

#[path = "support/task_harness.rs"]
mod task_harness;
use task_harness::TestTasks;

/// The internal node-to-node endpoints (`/v1/batch/run`, `/report`) require the
/// cluster service token; the harness configures one so the dispatch path (and
/// the direct tests below) can present it.
const TEST_SERVICE_TOKEN: &str = "rbrg_test_service_token";

#[derive(Default)]
struct HarnessOptions {
    council: Option<Arc<CouncilNode>>,
    node_name: Option<String>,
    membership: Option<Vec<NodeMembershipInfo>>,
    /// Nodes to report capacity for (name → the aggregated view).
    capacity_nodes: Vec<String>,
}

struct Harness {
    client: BunClient,
    base_url: String,
    port: u16,
    _tasks: TestTasks,
}

impl Harness {
    async fn start() -> Self {
        Self::start_with(HarnessOptions::default()).await
    }

    async fn start_with(options: HarnessOptions) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let shutdown = CancellationToken::new();

        let grill = ProcessGrill::new();
        let port_allocator = PortAllocator::new(42000, 43000);
        let agent_shutdown = shutdown.clone();
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, agent_shutdown);
        let deploy_history = agent.deploy_history_handle();

        let agent_task = tokio::spawn(async move {
            agent.run().await;
        });

        let aggregated = {
            let mut state = reliaburger::reporting::aggregator::AggregatedState::default();
            for name in &options.capacity_nodes {
                let node = reliaburger::meat::NodeId(name.clone());
                state.reports.insert(
                    node.clone(),
                    reliaburger::reporting::types::StateReport {
                        node_id: node,
                        timestamp: std::time::SystemTime::UNIX_EPOCH,
                        running_apps: vec![],
                        cached_specs: vec![],
                        resource_usage: reliaburger::reporting::types::ResourceUsage {
                            cpu_total_millicores: 8000,
                            cpu_used_millicores: 0,
                            memory_total_mb: 16384,
                            memory_used_mb: 0,
                            ..Default::default()
                        },
                        event_log: vec![],
                        has_buildah: false,
                    },
                );
            }
            state
        };
        let (_aggregated_tx, aggregated_rx) = tokio::sync::watch::channel(aggregated);
        let aggregated_rx = if options.capacity_nodes.is_empty() {
            None
        } else {
            Some(aggregated_rx)
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = api::router_with_upgrade(
            cmd_tx.clone(),
            None,
            None,
            Some(deploy_history),
            None,
            None,
            options.council,
            None,
            Some(TEST_SERVICE_TOKEN.to_string()),
            None,
            options
                .membership
                .map(|members| Arc::new(RwLock::new(members))),
            None,
            9117,
            None,
            None,
            aggregated_rx,
            options.node_name,
            900,
            reliaburger::cluster::ClusterHttp::plaintext(),
            5050,
            256 * 1024 * 1024,
            false,
        );
        let server_shutdown = shutdown.clone();
        let server_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { server_shutdown.cancelled().await })
                .await
                .ok();
        });

        let base_url = format!("http://127.0.0.1:{port}");
        let client = BunClient::new(&base_url);
        for _ in 0..20 {
            if client.health().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Self {
            client,
            base_url,
            port,
            _tasks: TestTasks::new(shutdown, vec![agent_task, server_task]),
        }
    }

    /// Poll the batch summary until `done` or the deadline.
    async fn wait_done(&self, batch_id: u64, secs: u64) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let summary = self.client.batch_status(batch_id).await.unwrap();
            if summary["done"].as_bool().unwrap_or(false) {
                return summary;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "batch {batch_id} not done in {secs}s: {summary}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn fast_config() -> CouncilConfig {
    CouncilConfig {
        heartbeat_interval_ms: 50,
        election_timeout_min_ms: 150,
        election_timeout_max_ms: 400,
        snapshot_threshold: 100,
        max_in_snapshot_log_to_keep: 50,
    }
}

/// A single-node council, initialised so it becomes leader.
async fn single_node_leader() -> Arc<CouncilNode> {
    let router = InMemoryRaftRouter::new();
    let network = InMemoryRaftNetworkFactory::new(1, router.clone());
    let node = CouncilNode::new(
        1,
        fast_config(),
        network,
        MemLogStore::new(),
        CouncilStateMachine::new(),
        None,
    )
    .await
    .unwrap();
    router.register(1, node.raft().clone()).await;
    let mut members = BTreeMap::new();
    members.insert(
        1u64,
        CouncilNodeInfo::new("127.0.0.1:9001".parse().unwrap(), "node-1".to_string()),
    );
    node.initialize(members).await.unwrap();

    let node = Arc::new(node);
    for _ in 0..40 {
        if node.is_leader().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    node
}

fn jobs_from(toml: &str) -> std::collections::BTreeMap<String, reliaburger::config::job::JobSpec> {
    Config::parse(toml).unwrap().job
}

/// Roadmap (Phase 12): submit a batch of process jobs; all run to
/// completion and the tracker reports them done.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_of_proc_jobs_completes_locally() {
    let harness = Harness::start().await;

    let jobs = jobs_from(
        r#"
        [job.quick-1]
        image = "proc-grill:image-ignored"
        command = ["echo", "one"]

        [job.quick-2]
        image = "proc-grill:image-ignored"
        command = ["echo", "two"]

        [job.quick-3]
        image = "proc-grill:image-ignored"
        command = ["echo", "three"]
    "#,
    );
    let response = harness.client.submit_batch(&jobs).await.unwrap();
    assert_eq!(response["assigned"].as_u64(), Some(3));
    assert!(
        response["unschedulable"].as_array().unwrap().is_empty(),
        "single-node fallback capacity must schedule everything"
    );

    let batch_id = response["batch_id"].as_u64().unwrap();
    let summary = harness.wait_done(batch_id, 30).await;
    assert_eq!(summary["completed"].as_u64(), Some(3), "{summary}");
    assert_eq!(summary["failed"].as_u64(), Some(0), "{summary}");
}

/// A job that runs and exits non-zero exhausts its retries (jobs get
/// `RestartPolicy::for_job(3)`) and is reported failed — the tracker
/// reaches a terminal state rather than pending forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_failing_job_reports_failed() {
    let harness = Harness::start().await;

    let jobs = jobs_from(
        r#"
        [job.doomed]
        image = "proc-grill:image-ignored"
        command = ["sh", "-c", "exit 1"]
    "#,
    );
    let response = harness.client.submit_batch(&jobs).await.unwrap();
    let batch_id = response["batch_id"].as_u64().unwrap();

    // 3 retries with 1s/2s/4s backoff — well inside 60s.
    let summary = harness.wait_done(batch_id, 60).await;
    assert_eq!(summary["failed"].as_u64(), Some(1), "{summary}");
    assert_eq!(summary["completed"].as_u64(), Some(0), "{summary}");
}

/// An empty batch is a client error, not a mysterious empty success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_batch_is_rejected() {
    let harness = Harness::start().await;
    let result = harness
        .client
        .submit_batch(&std::collections::BTreeMap::new())
        .await;
    assert!(result.is_err());
}

/// JOB3: a job in a non-default namespace completes — the namespace is
/// resolved once at submit, so the deploy and the completion watcher
/// agree on where to look. This used to strand the batch for an hour.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_in_a_non_default_namespace_completes() {
    let harness = Harness::start().await;

    let jobs = jobs_from(
        r#"
        [job.spaced]
        image = "proc-grill:image-ignored"
        namespace = "batchspace"
        command = ["echo", "hi"]
    "#,
    );
    let response = harness.client.submit_batch(&jobs).await.unwrap();
    let batch_id = response["batch_id"].as_u64().unwrap();
    let summary = harness.wait_done(batch_id, 30).await;
    assert_eq!(summary["completed"].as_u64(), Some(1), "{summary}");
}

/// JOB3: a submission whose namespace disagrees with the job spec's is
/// rejected — one authoritative namespace, or nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conflicting_namespaces_are_rejected_at_submit() {
    let harness = Harness::start().await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/batch", harness.base_url))
        .json(&serde_json::json!({
            "jobs": [{
                "name": "torn",
                "namespace": "one",
                "spec": {
                    "image": "proc-grill:image-ignored",
                    "namespace": "two",
                    "command": ["echo", "hi"],
                },
            }],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 400);
}

/// JOB3: unschedulable jobs appear in the batch result and its summary
/// instead of being silently omitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unschedulable_jobs_appear_in_the_batch() {
    let harness = Harness::start().await;

    // A job demanding more CPU than the fallback capacity can ever hold.
    let response = reqwest::Client::new()
        .post(format!("{}/v1/batch", harness.base_url))
        .json(&serde_json::json!({
            "jobs": [
                {
                    "name": "modest",
                    "spec": { "image": "proc-grill:image-ignored", "command": ["echo", "ok"] },
                },
                {
                    "name": "greedy",
                    "spec": {
                        "image": "proc-grill:image-ignored",
                        "command": ["echo", "never"],
                        // More millicores than the fallback capacity
                        // (u64::MAX / 2) can ever satisfy.
                        "cpu": "9999999999999999999",
                    },
                },
            ],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 202);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["assigned"].as_u64(), Some(1), "{body}");
    assert_eq!(
        body["unschedulable"],
        serde_json::json!(["greedy"]),
        "{body}"
    );

    let batch_id = body["batch_id"].as_u64().unwrap();
    let summary = harness.wait_done(batch_id, 30).await;
    assert_eq!(summary["unschedulable"].as_u64(), Some(1), "{summary}");
    assert_eq!(summary["completed"].as_u64(), Some(1), "{summary}");
    assert_eq!(summary["total"].as_u64(), Some(2), "{summary}");
}

/// The node-to-node half of dispatch: `/v1/batch/run` accepts a job
/// group, runs it, and posts its completion report to the callback
/// URL. (The leader-side grouping that produces these requests is
/// covered by the submit tests; the full two-node loop runs in the
/// cluster acceptance runbook.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_run_endpoint_runs_jobs_and_calls_back() {
    let submitter = Harness::start().await;
    let runner = Harness::start().await;

    let http = reqwest::Client::new();
    let run = serde_json::json!({
        "batch_id": 424242,
        "callback_base_url": submitter.base_url,
        "jobs": [{
            "name": "remote-1",
            "spec": { "image": "proc-grill:image-ignored", "command": ["echo", "remote"] },
        }],
    });
    let response = http
        .post(format!("{}/v1/batch/run", runner.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&run)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 202);

    // The runner actually executes the job…
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let statuses = runner.client.status().await.unwrap();
        if statuses
            .iter()
            .any(|s| s.app_name == "remote-1" && s.state == "stopped")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "remote job never completed: {statuses:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // …and the report endpoint validates: an unknown batch id is a 404
    // now that trackers are durable (JOB3/JOB4) — a leader restart no
    // longer loses batches, so "unknown" means forged or garbage.
    let report = http
        .post(format!("{}/v1/batch/424242/report", submitter.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&serde_json::json!({ "job_name": "remote-1", "status": "completed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(report.status().as_u16(), 404);
}

/// JOB3: `/v1/batch/{id}/report` validates its input — forged states
/// are 400, illegal transitions are 409, duplicates are an idempotent
/// 200 that never double-counts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_reports_are_validated_and_idempotent() {
    let harness = Harness::start().await;

    let jobs = jobs_from(
        r#"
        [job.steady]
        image = "proc-grill:image-ignored"
        command = ["echo", "hi"]
    "#,
    );
    let response = harness.client.submit_batch(&jobs).await.unwrap();
    let batch_id = response["batch_id"].as_u64().unwrap();
    harness.wait_done(batch_id, 30).await;

    let http = reqwest::Client::new();
    let report_url = format!("{}/v1/batch/{batch_id}/report", harness.base_url);

    // A made-up status string is a 400.
    let forged = http
        .post(&report_url)
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&serde_json::json!({ "job_name": "steady", "status": "meltdown" }))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status().as_u16(), 400);

    // A duplicate of the recorded terminal state is an idempotent 200.
    let duplicate = http
        .post(&report_url)
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&serde_json::json!({ "job_name": "steady", "status": "completed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status().as_u16(), 200);
    let body: serde_json::Value = duplicate.json().await.unwrap();
    assert_eq!(body["duplicate"], serde_json::json!(true));
    let summary = harness.client.batch_status(batch_id).await.unwrap();
    assert_eq!(summary["completed"].as_u64(), Some(1), "no double count");

    // A conflicting terminal state (completed → failed) is a 409.
    let conflict = http
        .post(&report_url)
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&serde_json::json!({ "job_name": "steady", "status": "failed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status().as_u16(), 409);

    // A job that was never part of the batch is a 404.
    let unknown = http
        .post(&report_url)
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&serde_json::json!({ "job_name": "impostor", "status": "completed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status().as_u16(), 404);
}

/// JOB1: the internal `/v1/batch/run` and `/report` endpoints reject a caller
/// that is not the system principal, so a ReadOnly/anonymous caller cannot run
/// work or forge completion (and never reaches the service-token callback).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_internal_endpoints_require_the_system_principal() {
    let runner = Harness::start().await;
    let http = reqwest::Client::new();
    let run = serde_json::json!({
        "batch_id": 1,
        "jobs": [{
            "name": "x",
            "spec": { "image": "proc-grill:image-ignored", "command": ["echo", "x"] },
        }],
    });

    // No token at all → not the system principal → 403.
    let no_token = http
        .post(format!("{}/v1/batch/run", runner.base_url))
        .json(&run)
        .send()
        .await
        .unwrap();
    assert_eq!(no_token.status().as_u16(), 403);

    // A non-service bearer token is also refused.
    let wrong = http
        .post(format!("{}/v1/batch/run", runner.base_url))
        .bearer_auth("rbrg_not_the_service_token")
        .json(&run)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status().as_u16(), 403);

    // The report endpoint is equally guarded.
    let report = http
        .post(format!("{}/v1/batch/1/report", runner.base_url))
        .json(&serde_json::json!({ "job_name": "x", "status": "completed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(report.status().as_u16(), 403);
}

/// JOB6 residue: the CLI's wait loop is bounded — a batch that never
/// finishes ends the wait with a non-zero error carrying the last
/// known state, not an infinite poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_batch_wait_times_out_with_the_last_known_state() {
    let harness = Harness::start().await;

    let jobs = jobs_from(
        r#"
        [job.slowpoke]
        image = "proc-grill:image-ignored"
        command = ["sleep", "300"]
    "#,
    );
    let response = harness.client.submit_batch(&jobs).await.unwrap();
    let batch_id = response["batch_id"].as_u64().unwrap();

    let err = reliaburger::relish::commands::wait_for_batch(
        &harness.client,
        batch_id,
        Duration::from_secs(2),
    )
    .await
    .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("timed out"), "{message}");
    assert!(message.contains("last known state"), "{message}");
}

/// JOB4: with a council, batch ids come from the replicated counter —
/// a fresh API process (a restarted leader) never reuses them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_ids_stay_monotonic_across_an_api_restart() {
    let council = single_node_leader().await;

    let first = Harness::start_with(HarnessOptions {
        council: Some(Arc::clone(&council)),
        node_name: Some("leader".to_string()),
        ..Default::default()
    })
    .await;
    let jobs = jobs_from(
        r#"
        [job.first]
        image = "proc-grill:image-ignored"
        command = ["echo", "one"]
    "#,
    );
    let response = first.client.submit_batch(&jobs).await.unwrap();
    let first_id = response["batch_id"].as_u64().unwrap();
    first.wait_done(first_id, 30).await;
    drop(first);

    let second = Harness::start_with(HarnessOptions {
        council: Some(Arc::clone(&council)),
        node_name: Some("leader".to_string()),
        ..Default::default()
    })
    .await;
    // The old batch is still readable after the "restart"…
    let summary = second.client.batch_status(first_id).await.unwrap();
    assert_eq!(summary["completed"].as_u64(), Some(1), "{summary}");
    // …and a new submission gets a strictly newer id.
    let response = second.client.submit_batch(&jobs).await.unwrap();
    let second_id = response["batch_id"].as_u64().unwrap();
    assert!(second_id > first_id, "{second_id} vs {first_id}");
}

/// JOB3/JOB4: a lost completion callback cannot strand a batch. The
/// runner's callbacks go to a dead port (the poisoned membership entry
/// for the leader), so every push report is lost; the leader's pull
/// watcher polls the runner's status API and completes the batch
/// anyway. A fresh API process over the same council exercises the
/// restart-resume path: the watcher respawns from the durable record
/// on the first status read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_callback_batch_still_terminates_via_the_pull_watcher() {
    let council = single_node_leader().await;

    // A dead port for the leader: callbacks to it are lost.
    let dead_leader_address: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();

    // The runner: a plain worker node, no council. Its membership table
    // lists the (dead) leader so the callback URL passes the
    // known-member check — and then every delivery attempt fails.
    let runner = Harness::start_with(HarnessOptions {
        node_name: Some("runner".to_string()),
        membership: Some(vec![NodeMembershipInfo {
            node_id: reliaburger::meat::NodeId("leader".to_string()),
            address: dead_leader_address,
        }]),
        ..Default::default()
    })
    .await;
    let runner_address: std::net::SocketAddr =
        format!("127.0.0.1:{}", runner.port).parse().unwrap();

    let membership = vec![
        NodeMembershipInfo {
            node_id: reliaburger::meat::NodeId("leader".to_string()),
            address: dead_leader_address,
        },
        NodeMembershipInfo {
            node_id: reliaburger::meat::NodeId("runner".to_string()),
            address: runner_address,
        },
    ];

    let leader = Harness::start_with(HarnessOptions {
        council: Some(Arc::clone(&council)),
        node_name: Some("leader".to_string()),
        membership: Some(membership),
        capacity_nodes: vec!["runner".to_string()],
    })
    .await;

    // Submit: the only capacity is the runner, so the job dispatches
    // there with a callback URL pointing at the dead port.
    let jobs = jobs_from(
        r#"
        [job.faraway]
        image = "proc-grill:image-ignored"
        command = ["echo", "far"]
    "#,
    );
    let response = leader.client.submit_batch(&jobs).await.unwrap();
    let batch_id = response["batch_id"].as_u64().unwrap();
    assert_eq!(response["assigned"].as_u64(), Some(1), "{response}");

    // The pull watcher completes the batch despite the lost callbacks.
    let summary = leader.wait_done(batch_id, 60).await;
    assert_eq!(summary["completed"].as_u64(), Some(1), "{summary}");

    // Leader "restart" mid-history: a fresh process over the same
    // council still serves the batch from the durable record.
    drop(leader);
    let restarted = Harness::start_with(HarnessOptions {
        council: Some(Arc::clone(&council)),
        node_name: Some("leader".to_string()),
        ..Default::default()
    })
    .await;
    let summary = restarted.client.batch_status(batch_id).await.unwrap();
    assert_eq!(summary["completed"].as_u64(), Some(1), "{summary}");
}

/// JOB4: a leader restart mid-batch resumes watching from the durable
/// record. The batch is registered in Raft with its job assigned to a
/// runner node and no watcher alive anywhere (the "old leader" died
/// right after dispatch). The job completes on the runner; a status
/// read on the "new leader" spawns the pull watcher, which finds the
/// terminal state and finishes the batch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leader_restart_mid_batch_resumes_from_the_durable_record() {
    let council = single_node_leader().await;

    let runner = Harness::start_with(HarnessOptions {
        node_name: Some("runner".to_string()),
        ..Default::default()
    })
    .await;
    let runner_address: std::net::SocketAddr =
        format!("127.0.0.1:{}", runner.port).parse().unwrap();

    // The "old leader" registered this batch and dispatched, then died.
    let record = BatchRecord {
        jobs: vec![BatchJobRecord {
            name: "orphan".to_string(),
            namespace: "default".to_string(),
            node: Some(reliaburger::meat::NodeId("runner".to_string())),
            status: JobStatus::Pending,
        }],
        submitted_at_epoch_secs: epoch_now_secs(),
    };
    let response = council
        .write(reliaburger::council::types::RaftRequest::BatchRegister { batch: record })
        .await
        .unwrap();
    let batch_id = match response {
        reliaburger::council::types::CouncilResponse::BatchRegistered { batch_id } => batch_id,
        other => panic!("unexpected response: {other:?}"),
    };

    // The dispatched share is running (and completing) on the runner,
    // reporting to nobody: its callback target is gone.
    let http = reqwest::Client::new();
    let run = serde_json::json!({
        "batch_id": batch_id,
        "callback_base_url": null,
        "jobs": [{
            "name": "orphan",
            "spec": { "image": "proc-grill:image-ignored", "command": ["echo", "orphan"] },
        }],
    });
    let accepted = http
        .post(format!("{}/v1/batch/run", runner.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&run)
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status().as_u16(), 202);

    // The "new leader" starts fresh, knows the batch only from Raft,
    // and resumes watching on the first status read.
    let membership = vec![NodeMembershipInfo {
        node_id: reliaburger::meat::NodeId("runner".to_string()),
        address: runner_address,
    }];
    let new_leader = Harness::start_with(HarnessOptions {
        council: Some(Arc::clone(&council)),
        node_name: Some("leader".to_string()),
        membership: Some(membership),
        ..Default::default()
    })
    .await;
    let summary = new_leader.wait_done(batch_id, 60).await;
    assert_eq!(summary["completed"].as_u64(), Some(1), "{summary}");
}
