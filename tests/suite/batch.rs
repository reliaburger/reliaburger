//! Common finite-job admission through the public API, Raft and owned runtime.
//! Requests queue durable count-one runs; worker CPU, memory and concurrency
//! budgets govern physical starts. Accepted worker ledgers determine outcomes.
//! The retained internal batch protocol has separate migration regressions:
//! execution aliases, callbacks and app/prerequisite ownership cannot cross
//! into a common run or erase an uncertain older execution.

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
use reliaburger::relish::client::BunClient;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::admission_fixture::write_admission_fixture;
use crate::task_harness;
use task_harness::TestTasks;

/// Node-to-node endpoints (`/v1/batch/array/sync`) require the cluster service
/// token; the harness configures one so the tests below can present it.
const TEST_SERVICE_TOKEN: &str = "rbrg_test_service_token";

#[derive(Default)]
struct HarnessOptions {
    council: Option<Arc<CouncilNode>>,
    node_name: Option<String>,
    membership: Option<Vec<NodeMembershipInfo>>,
    /// Nodes to report capacity for (name → the aggregated view).
    capacity_nodes: Vec<String>,
    stale_capacity_nodes: Vec<String>,
    close_capacity_channel: bool,
    aggregated_override:
        Option<tokio::sync::watch::Receiver<reliaburger::reporting::aggregator::AggregatedState>>,
    /// Trusted authentication context injected by this test server's boundary.
    auth_context: Option<reliaburger::sesame::auth::AuthContext>,
    log_sink: Option<mpsc::Sender<reliaburger::ketchup::types::LogRecord>>,
    log_store: Option<Arc<RwLock<reliaburger::ketchup::log_store::LogStore>>>,
    records_dir: Option<std::path::PathBuf>,
}

pub(crate) struct Harness {
    client: BunClient,
    pub(crate) base_url: String,
    port: u16,
    _tasks: TestTasks,
    _capacity_publisher:
        Option<tokio::sync::watch::Sender<reliaburger::reporting::aggregator::AggregatedState>>,
}

impl Harness {
    pub(crate) async fn start() -> Self {
        Self::start_with(HarnessOptions::default()).await
    }

    async fn start_with(options: HarnessOptions) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let shutdown = CancellationToken::new();

        let grill = ProcessGrill::new();
        let port_allocator = PortAllocator::new(42000, 43000);
        let agent_shutdown = shutdown.clone();
        let mut agent = BunAgent::new(
            reliaburger::grill::AnyGrill::Process(grill),
            port_allocator,
            cmd_rx,
            agent_shutdown,
        );
        let process_policy = reliaburger::config::process_workloads::ProcessWorkloadsConfig {
            allowed_binaries: [
                "/bin/sh",
                "/bin/echo",
                "/usr/bin/true",
                "/bin/true",
                "/usr/bin/false",
                "/bin/false",
                "/usr/bin/printf",
                "/bin/sleep",
            ]
            .into_iter()
            .map(Into::into)
            .collect(),
            mount_isolation: false,
            ..Default::default()
        };
        agent.set_process_config(process_policy.clone());
        agent.set_node_capacity(8000, 16384);
        if let Some(directory) = options.records_dir {
            agent.set_records_dir(directory);
            agent.adopt_recorded_instances().await.unwrap();
        }
        if let Some(sink) = options.log_sink {
            agent.set_log_sink(sink, Default::default());
        }
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());
        let deploy_history = agent.deploy_history_handle();
        let status_reader = agent.status_reader();

        let job_data = tempfile::tempdir().unwrap();
        let runner = agent.delegated_task_runner(job_data.path()).unwrap();
        let node = reliaburger::bun::task_array_node::TaskArrayNode::new(
            reliaburger::bun::task_array_node::TaskArrayNodeConfig {
                root: job_data.path().join("task-arrays"),
                policy: process_policy,
                default_concurrency: 8,
                backoff: (Duration::from_millis(1), Duration::from_millis(5)),
                group_commit: Default::default(),
            },
            reliaburger::bun::task_array_node::NodeRunner::Owned(Box::new(runner)),
        )
        .with_budget(agent.execution_budget());
        let task_arrays = Arc::new(
            reliaburger::bun::task_array_leader::TaskArrayService::with_timings(
                Some(Arc::new(node)),
                Duration::from_millis(50),
                Duration::from_secs(3),
            )
            .with_storage(job_data.path())
            .await
            .unwrap(),
        );
        let agent_task = tokio::spawn(async move {
            agent.run().await;
            drop(agent);
            drop(volumes);
        });

        let aggregated = {
            let mut state = reliaburger::reporting::aggregator::AggregatedState {
                leadership_epoch: options
                    .council
                    .as_ref()
                    .map(|council| council.current_term()),
                ..Default::default()
            };
            for name in &options.capacity_nodes {
                let node = reliaburger::meat::NodeId(name.clone());
                state.receive_deadlines.insert(
                    node.clone(),
                    tokio::time::Instant::now() + Duration::from_secs(30),
                );
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
                        job_reservations: Default::default(),
                        capacity_refusals: Vec::new(),
                    },
                );
            }
            for name in &options.stale_capacity_nodes {
                state.stale_nodes.push(reliaburger::meat::NodeId::new(name));
            }
            state
        };
        let (_aggregated_tx, aggregated_rx) = tokio::sync::watch::channel(aggregated);
        let aggregated_rx = if options.aggregated_override.is_some() {
            options.aggregated_override
        } else if options.capacity_nodes.is_empty() {
            None
        } else {
            Some(aggregated_rx)
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = api::router_with_upgrade(
            cmd_tx.clone(),
            None,
            options.log_store,
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
            None,
            9117,
            None,
            None,
            aggregated_rx,
            "default".to_string(),
            options.node_name,
            reliaburger::bun::build_runner::BuildSettings::with_timeout(900),
            reliaburger::cluster::ClusterHttp::plaintext(),
            5050,
            "http",
            256 * 1024 * 1024,
            false,
            reliaburger::bun::capabilities::StaticCapabilities::default(),
            reliaburger::bun::readiness::ReadinessTracker::new(),
            None,
            None,
            Some(status_reader),
            Some(task_arrays),
        );
        let app = match options.auth_context {
            Some(auth) => app.layer(axum::middleware::from_fn(
                move |mut request: axum::extract::Request, next: axum::middleware::Next| {
                    let auth = auth.clone();
                    async move {
                        request.extensions_mut().insert(auth);
                        next.run(request).await
                    }
                },
            )),
            None => app,
        };
        let server_shutdown = shutdown.clone();
        let server_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { server_shutdown.cancelled().await })
                .await
                .ok();
            drop(job_data);
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
            _capacity_publisher: (!options.close_capacity_channel).then_some(_aggregated_tx),
        }
    }

    /// Poll the batch summary until `done` or the deadline.
    pub(crate) async fn wait_done(&self, batch_id: u64, secs: u64) -> serde_json::Value {
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

/// A running API's leader loop commits entries of its own: its first tick
/// records a cron observation. A seeded admission entry planned just before
/// such a write is refused with "admission revision changed" (#735), so the
/// fixture must replan from committed state, as the scheduler does. The
/// writer here commits about every millisecond, far more often than the
/// leader loop, so a fixture that plans only once is refused within a hundred
/// rounds (19 runs out of 20 when this was written).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_admission_fixture_replans_when_another_entry_lands_first() {
    use reliaburger::council::types::{CouncilResponse, RaftRequest};
    use reliaburger::meat::types::{Placement, SchedulingDecision};

    let council = single_node_leader().await;
    let stop = CancellationToken::new();
    let writer = {
        let council = council.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            while !stop.is_cancelled() {
                council.write(RaftRequest::Noop).await.unwrap();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
    };
    let mut applied = 0;
    for round in 0..100 {
        let decision = SchedulingDecision {
            app_id: reliaburger::meat::AppId::new(format!("seed-{round}"), "default"),
            placements: vec![Placement {
                node_id: reliaburger::meat::NodeId::new("worker"),
                resources: reliaburger::meat::Resources::default(),
                ordinal: 0,
            }],
        };
        let response = write_admission_fixture(&council, RaftRequest::SchedulingDecision(decision))
            .await
            .unwrap();
        if matches!(response, CouncilResponse::Applied { .. }) {
            applied += 1;
        }
    }
    stop.cancel();
    writer.await.unwrap();
    council.shutdown().await.unwrap();
    assert_eq!(
        applied, 100,
        "seeded decisions refused by a competing writer"
    );
}

fn jobs_from(toml: &str) -> std::collections::BTreeMap<String, reliaburger::config::job::JobSpec> {
    Config::parse(toml).unwrap().job
}

fn capacity_options(council: Arc<CouncilNode>) -> HarnessOptions {
    HarnessOptions {
        council: Some(council),
        node_name: Some("node-1".into()),
        membership: Some(vec![NodeMembershipInfo {
            node_id: reliaburger::meat::NodeId::new("node-1"),
            address: "127.0.0.1:9001".parse().unwrap(),
            api_advertised: true,
        }]),
        capacity_nodes: vec!["node-1".into()],
        ..Default::default()
    }
}

fn local_capacity_options(council: Arc<CouncilNode>, name: &str) -> HarnessOptions {
    let mut options = capacity_options(council);
    options.node_name = Some(name.into());
    options.membership.as_mut().unwrap()[0].node_id = reliaburger::meat::NodeId::new(name);
    options.capacity_nodes = vec![name.into()];
    options
}

/// Two API processes share Raft, but have independent handlers and the same
/// lagging worker report. Their admissions must still share one reservation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_api_admission_deduplicates_the_same_durable_batch_request() {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let council = single_node_leader().await;
    let (first_tx, _first_rx) = mpsc::channel(16);
    let (second_tx, _second_rx) = mpsc::channel(16);
    // Independent API state, one authoritative council, no synthetic child exit.
    let first = api::router(
        first_tx,
        None,
        None,
        None,
        None,
        None,
        Some(council.clone()),
        None,
        None,
        None,
        None,
        None,
        0,
        None,
    );
    let second = api::router(
        second_tx,
        None,
        None,
        None,
        None,
        None,
        Some(council.clone()),
        None,
        None,
        None,
        None,
        None,
        0,
        None,
    );
    let body = serde_json::json!({"jobs":[{"name":"same-request","spec":{"runtime":"process","exec":"/usr/bin/true","command":[]}}]}).to_string();
    let request = || {
        Request::post("/v1/batch")
            .header("content-type", "application/json")
            .header("idempotency-key", "same-durable-request")
            .body(Body::from(body.clone()))
            .unwrap()
    };
    let (a, b) = tokio::join!(
        first.clone().oneshot(request()),
        second.clone().oneshot(request())
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.status(), 202);
    assert_eq!(b.status(), 202);
    let a: serde_json::Value =
        serde_json::from_slice(&a.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let b: serde_json::Value =
        serde_json::from_slice(&b.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(a["batch_id"], b["batch_id"]);
    let before = council.desired_state().await.task_arrays;
    assert_eq!(before.jobs().runs().count(), 1);
    assert_eq!(before.manifests().count(), 1);
    let id = before.jobs().runs().next().unwrap().0;
    assert_eq!(before.get(id).unwrap().state.summary().queued, 1);
    let changed = Request::post("/v1/batch")
        .header("content-type", "application/json")
        .header("idempotency-key", "same-durable-request")
        .body(Body::from(body.replace("true", "false")))
        .unwrap();
    assert_eq!(second.oneshot(changed).await.unwrap().status(), 409);
    assert_eq!(council.desired_state().await.task_arrays, before);
    drop(first);
    council.shutdown().await.unwrap();
}

async fn assert_batch_refuses_declarative_job_field(field: &str) {
    let council = single_node_leader().await;
    let scratch = tempfile::tempdir().unwrap();
    let launches = scratch.path().join("launches");
    let records_dir = scratch.path().join("records");
    let runner = Harness::start_with(HarnessOptions {
        council: Some(council.clone()),
        node_name: Some("node-1".into()),
        records_dir: Some(records_dir.clone()),
        ..Default::default()
    })
    .await;
    let mut spec = serde_json::json!({
        "image": "proc-grill:image-ignored", "namespace": "default",
        "command": ["sh", "-c", format!("printf 'launch\\n' >> '{}'", launches.display())]
    });
    spec[field] = match field {
        "schedule" => serde_json::json!("* * * * *"),
        "run_before" => serde_json::json!(["app.web"]),
        _ => unreachable!(),
    };
    let request = serde_json::json!({
        "jobs": [{"name": "migration", "namespace": "default", "spec": spec}]
    });
    let response = reqwest::Client::new()
        .post(format!("{}/v1/batch", runner.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "batch accepted unsupported declarative field {field}"
    );
    assert!(!records_dir.join("job-attempts.checkpoint").exists());
    assert!(!launches.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_batch_refuses_a_cron_schedule_before_registration() {
    assert_batch_refuses_declarative_job_field("schedule").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_batch_refuses_run_before_without_an_app_submission() {
    assert_batch_refuses_declarative_job_field("run_before").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_apply_cannot_supply_a_batch_execution_label() {
    let harness = Harness::start().await;
    for field in ["logical_name='migration'", "batch_execution=true"] {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/apply", harness.base_url))
            .body(format!(
                "[job.batch-111]\nruntime = \"process\"\nexec = \"/usr/bin/true\"\ncommand=[]\n{field}\n"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn a_previous_named_run_cannot_complete_a_new_batch_before_dispatch_acknowledges() {
    use http_body_util::BodyExt;
    use reliaburger::bun::agent::{AgentCommand, InstanceStatus};
    use tower::ServiceExt;

    let (tx, mut rx) = mpsc::channel(32);
    let (observed_tx, mut observed_rx) = mpsc::channel(8);
    let actor = tokio::spawn(async move {
        let mut held_events = Vec::new();
        while let Some(command) = rx.recv().await {
            match command {
                AgentCommand::Deploy { events, .. } => held_events.push(events),
                AgentCommand::Status { response } => {
                    let _ = response.send(vec![InstanceStatus {
                        id: "previous-run".into(),
                        app_name: "migration".into(),
                        namespace: "default".into(),
                        state: "stopped".into(),
                        restart_count: 0,
                        host_port: None,
                        exit_code: Some(0),
                        pid: None,
                        runtime_unknown: false,
                        status_age_ms: None,
                    }]);
                    let _ = observed_tx.send(()).await;
                }
                _ => {}
            }
        }
    });
    let council = single_node_leader().await;
    let router = api::router(
        tx,
        None,
        None,
        None,
        None,
        None,
        Some(council.clone()),
        None,
        None,
        None,
        None,
        None,
        0,
        None,
    );
    let request = axum::http::Request::builder().method("POST").uri("/v1/batch")
        .header("content-type", "application/json").body(axum::body::Body::from(r#"{"jobs":[{"name":"migration","spec":{"image":"busybox","command":["sleep","60"]}}]}"#)).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let submitted: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = submitted["batch_id"].as_u64().unwrap();
    // Explicit status pulls expose the older stopped process. Neither this
    // inventory nor a legacy callback is proof of the new run's completion.
    for _ in 0..2 {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::get("/v1/status")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_success());
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&bytes).contains("previous-run"));
        observed_rx.recv().await.unwrap();
    }
    let request = axum::http::Request::builder()
        .uri(format!("/v1/batch/{id}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let summary: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    actor.abort();
    assert_eq!(summary["succeeded"], 0, "{summary}");
    assert_eq!(summary["done"], false, "{summary}");
    assert_eq!(summary["queued"], 1, "{summary}");
    council.shutdown().await.unwrap();
}

/// Roadmap (Phase 12): submit a batch of process jobs; all run to
/// completion and the tracker reports them done.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_batches_with_the_same_label_run_independent_jobs() {
    let harness = Harness::start().await;
    let jobs = jobs_from(
        r#"[job.shared]
runtime = "process"
exec = "/bin/sleep"
command = ["1"]
"#,
    );
    let first = harness.client.submit_batch(&jobs, None).await.unwrap();
    let second = harness.client.submit_batch(&jobs, None).await.unwrap();
    assert_ne!(
        first["executions"]["shared"],
        second["executions"]["shared"]
    );
    for response in [first, second] {
        let summary = harness
            .wait_done(response["batch_id"].as_u64().unwrap(), 30)
            .await;
        assert_eq!(summary["succeeded"], 1, "{summary}");
        assert_eq!(summary["failed"], 0, "{summary}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_maximum_length_batch_label_gets_a_valid_independent_execution_name() {
    let harness = Harness::start().await;
    let label = "m".repeat(63);
    let jobs = jobs_from(&format!(
        "[job.{label}]\nruntime = \"process\"\nexec = \"/usr/bin/true\"\ncommand=[]\n"
    ));
    let response = harness.client.submit_batch(&jobs, None).await.unwrap();
    let execution = response["executions"][&label]
        .as_str()
        .expect("submission must expose its independent execution identity");
    assert!(
        execution.len() <= 63,
        "execution name exceeds a DNS label: {execution}"
    );
    assert!(
        !execution.starts_with(&label),
        "logical name was concatenated into runtime identity"
    );
    assert_eq!(response["assigned"].as_u64(), Some(1));
    let result = harness
        .wait_done(response["batch_id"].as_u64().unwrap(), 10)
        .await;
    assert_eq!(result["succeeded"].as_u64(), Some(1), "{result}");
}

/// Roadmap (Phase 12): submit a batch of process jobs; all run to
/// completion and the tracker reports them done.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_of_proc_jobs_completes_locally() {
    let harness = Harness::start().await;

    let jobs = jobs_from(
        r#"
        [job.quick-1]
        runtime = "process"
        exec = "/bin/echo"
        command = ["one"]

        [job.quick-2]
        runtime = "process"
        exec = "/bin/echo"
        command = ["two"]

        [job.quick-3]
        runtime = "process"
        exec = "/bin/echo"
        command = ["three"]
    "#,
    );
    let response = harness.client.submit_batch(&jobs, None).await.unwrap();
    assert_eq!(response["assigned"].as_u64(), Some(3));
    assert!(
        response["unschedulable"].as_array().unwrap().is_empty(),
        "single-node fallback capacity must schedule everything"
    );

    let batch_id = response["batch_id"].as_u64().unwrap();
    let summary = harness.wait_done(batch_id, 30).await;
    assert_eq!(summary["succeeded"].as_u64(), Some(3), "{summary}");
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
        runtime = "process"
        exec = "/bin/sh"
        command = ["-c", "exit 1"]
    "#,
    );
    let response = harness.client.submit_batch(&jobs, None).await.unwrap();
    let batch_id = response["batch_id"].as_u64().unwrap();

    // 3 retries with 1s/2s/4s backoff — well inside 60s.
    let summary = harness.wait_done(batch_id, 60).await;
    assert_eq!(summary["failed"].as_u64(), Some(1), "{summary}");
    assert_eq!(summary["succeeded"].as_u64(), Some(0), "{summary}");
}

/// An empty batch is a client error, not a mysterious empty success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_batch_is_rejected() {
    let harness = Harness::start().await;
    let result = harness
        .client
        .submit_batch(&std::collections::BTreeMap::new(), None)
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
        runtime = "process"
        exec = "/bin/echo"
        namespace = "batchspace"
        command = ["hi"]
    "#,
    );
    let response = harness.client.submit_batch(&jobs, None).await.unwrap();
    let batch_id = response["batch_id"].as_u64().unwrap();
    let summary = harness.wait_done(batch_id, 30).await;
    assert_eq!(summary["succeeded"].as_u64(), Some(1), "{summary}");
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
                    "runtime":"process","exec":"/bin/echo",
                    "namespace": "two",
                    "command": ["hi"],
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
                    "spec": { "runtime":"process","exec":"/bin/echo", "command": ["ok"] },
                },
                {
                    "name": "greedy",
                    "spec": {
                        "runtime":"process","exec":"/bin/echo",
                        "command": ["never"],
                        // More millicores than the fallback capacity
                        // (u64::MAX / 2) can ever satisfy.
                        "cpu": "9999999999999999999m",
                    },
                },
            ],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 202);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["assigned"].as_u64(), Some(2), "{body}");
    assert_eq!(body["unschedulable"], serde_json::json!([]), "{body}");
    let batch_id = body["batch_id"].as_u64().unwrap();
    let summary = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let summary = harness.client.batch_status(batch_id).await.unwrap();
            if summary["succeeded"] == 1 {
                break summary;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(summary["done"], false, "{summary}");
    assert_eq!(summary["queued"], 1, "{summary}");
    assert_eq!(summary["failed"], 0, "{summary}");
    harness.client.cancel_batch(batch_id).await.unwrap();
    let summary = harness.wait_done(batch_id, 10).await;
    assert_eq!(summary["succeeded"], 1, "{summary}");
    assert_eq!(summary["not_run"], 1, "{summary}");
    assert_eq!(summary["total"], 2, "{summary}");
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
        runtime = "process"
        exec = "/bin/sleep"
        command = ["300"]
    "#,
    );
    let response = harness.client.submit_batch(&jobs, None).await.unwrap();
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
        ..local_capacity_options(council.clone(), "leader")
    })
    .await;
    let jobs = jobs_from(
        r#"
        [job.first]
        runtime = "process"
        exec = "/bin/echo"
        command = ["one"]
    "#,
    );
    let response = first.client.submit_batch(&jobs, None).await.unwrap();
    let first_id = response["batch_id"].as_u64().unwrap();
    first.wait_done(first_id, 30).await;
    drop(first);

    let second = Harness::start_with(HarnessOptions {
        council: Some(Arc::clone(&council)),
        node_name: Some("leader".to_string()),
        ..local_capacity_options(council.clone(), "leader")
    })
    .await;
    // The old batch is still readable after the "restart"…
    let summary = second.client.batch_status(first_id).await.unwrap();
    assert_eq!(summary["succeeded"].as_u64(), Some(1), "{summary}");
    // …and a new submission gets a strictly newer id.
    let response = second.client.submit_batch(&jobs, None).await.unwrap();
    let second_id = response["batch_id"].as_u64().unwrap();
    assert!(second_id > first_id, "{second_id} vs {first_id}");
}

// Batch allocations bypass app placements. Follow routes must select the
// committed execution owner and retain the original logical scope boundary.

async fn assert_cluster_prerequisite_gate(exit_code: i32, overlap: bool) {
    let council = single_node_leader().await;
    let old = Config::parse("[app.api]\nimage='old'\n")
        .unwrap()
        .app
        .remove("api")
        .unwrap();
    council
        .write(reliaburger::council::types::RaftRequest::AppSpec {
            app_id: reliaburger::meat::AppId::new("api", "default"),
            spec: Box::new(old),
        })
        .await
        .unwrap();
    let harness = Harness::start_with(HarnessOptions {
        council: Some(council.clone()),
        ..Default::default()
    })
    .await;
    let scratch = tempfile::tempdir().unwrap();
    let started = scratch.path().join("started");
    let release = scratch.path().join("release");
    let command = format!(
        "touch '{}'; while [ ! -e '{}' ]; do sleep 0.02; done; exit {exit_code}",
        started.display(),
        release.display()
    );
    let mut config = Config::parse("[app.api]\nimage='new'\n[job.migrate]\nruntime = \"process\"\nexec = \"/bin/sh\"\nrun_before=['app.api']\n").unwrap();
    config.job.get_mut("migrate").unwrap().command = Some(vec!["-c".into(), command]);
    let client = harness.client.clone();
    let apply = tokio::spawn(async move { client.apply(&config).await });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let while_blocked = council.desired_state().await.apps
        [&reliaburger::meat::AppId::new("api", "default")]
        .image
        .clone();
    let overlapping_result = if overlap {
        Some(
            tokio::time::timeout(
                Duration::from_secs(3),
                harness
                    .client
                    .apply(&Config::parse("[app.api]\nimage='competing'\n").unwrap()),
            )
            .await
            .expect("an overlapping apply must be refused while the migration owns its app"),
        )
    } else {
        None
    };
    std::fs::write(&release, b"release").unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), apply)
        .await
        .unwrap()
        .unwrap();
    if let Some(overlapping_result) = overlapping_result {
        assert!(
            overlapping_result.is_err(),
            "overlapping cluster apply bypassed migration ownership: {overlapping_result:?}"
        );
    }
    assert_eq!(
        while_blocked.as_deref(),
        Some("old"),
        "new app became schedulable while the migration was blocked"
    );
    let after = council.desired_state().await.apps
        [&reliaburger::meat::AppId::new("api", "default")]
        .image
        .clone();
    if exit_code == 0 {
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(after.as_deref(), Some("new"));
    } else {
        assert!(result.is_err(), "{result:?}");
        assert_eq!(after.as_deref(), Some("old"));
    }
    drop(harness);
    council.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_apply_waits_for_prerequisite_success_before_committing_an_app_revision() {
    assert_cluster_prerequisite_gate(0, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_apply_refuses_a_failed_prerequisite_without_committing_an_app_revision() {
    assert_cluster_prerequisite_gate(1, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_cluster_prerequisite_owns_its_app_until_desired_writes_finish() {
    assert_cluster_prerequisite_gate(0, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follower_forwarding_cannot_bypass_a_failed_cluster_prerequisite() {
    let network = InMemoryRaftRouter::new();
    let mut councils = Vec::new();
    for id in [1, 2] {
        let node = Arc::new(
            CouncilNode::new(
                id,
                fast_config(),
                InMemoryRaftNetworkFactory::new(id, network.clone()),
                MemLogStore::new(),
                CouncilStateMachine::new(),
                None,
            )
            .await
            .unwrap(),
        );
        network.register(id, node.raft().clone()).await;
        councils.push(node);
    }
    councils[0]
        .initialize(BTreeMap::from([(
            1,
            CouncilNodeInfo::new("127.0.0.1:9001".parse().unwrap(), "node-1"),
        )]))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !councils[0].is_leader().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    councils[0]
        .add_learner(
            2,
            CouncilNodeInfo::new("127.0.0.1:9002".parse().unwrap(), "node-2"),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while councils[1].current_leader().await != Some(1) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let app_id = reliaburger::meat::AppId::new("api", "default");
    councils[0]
        .write(reliaburger::council::types::RaftRequest::AppSpec {
            app_id: app_id.clone(),
            spec: Box::new(
                Config::parse("[app.api]\nimage='old'\n")
                    .unwrap()
                    .app
                    .remove("api")
                    .unwrap(),
            ),
        })
        .await
        .unwrap();
    let leader = Harness::start_with(HarnessOptions {
        council: Some(councils[0].clone()),
        node_name: Some("node-1".into()),
        ..Default::default()
    })
    .await;
    let follower = Harness::start_with(HarnessOptions {
        council: Some(councils[1].clone()),
        node_name: Some("node-2".into()),
        membership: Some(vec![NodeMembershipInfo {
            node_id: reliaburger::meat::NodeId::new("node-1"),
            address: format!("127.0.0.1:{}", leader.port).parse().unwrap(),
            api_advertised: true,
        }]),
        ..Default::default()
    })
    .await;
    let scratch = tempfile::tempdir().unwrap();
    let started = scratch.path().join("started");
    let release = scratch.path().join("release");
    let mut config = Config::parse("[app.api]\nimage='new'\n[job.migrate]\nruntime = \"process\"\nexec = \"/bin/sh\"\nrun_before=['app.api']\n").unwrap();
    config.job.get_mut("migrate").unwrap().command = Some(vec![
        "-c".into(),
        format!(
            "touch '{}'; while [ ! -e '{}' ]; do sleep 0.02; done; exit 1",
            started.display(),
            release.display()
        ),
    ]);
    let client = follower.client.clone();
    let apply = tokio::spawn(async move { client.apply(&config).await });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let blocked_image = councils[0].desired_state().await.apps[&app_id]
        .image
        .clone();
    std::fs::write(release, b"release").unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), apply)
        .await
        .unwrap()
        .unwrap();
    let final_image = councils[0].desired_state().await.apps[&app_id]
        .image
        .clone();
    drop(follower);
    drop(leader);
    for council in councils {
        council.shutdown().await.unwrap();
    }
    assert_eq!(blocked_image.as_deref(), Some("old"));
    assert!(
        result.is_err(),
        "a forwarded failed migration was accepted: {result:?}"
    );
    assert_eq!(final_image.as_deref(), Some("old"));
}

async fn assert_handover_keeps_an_uncertain_prerequisite_claim(mode: &str) {
    use reliaburger::council::types::RaftRequest;
    use reliaburger::meat::AppId;

    let network = InMemoryRaftRouter::new();
    let mut councils = Vec::new();
    for id in [1, 2] {
        let node = Arc::new(
            CouncilNode::new(
                id,
                fast_config(),
                InMemoryRaftNetworkFactory::new(id, network.clone()),
                MemLogStore::new(),
                CouncilStateMachine::new(),
                None,
            )
            .await
            .unwrap(),
        );
        network.register(id, node.raft().clone()).await;
        councils.push(node);
    }
    councils[0]
        .initialize(BTreeMap::from([(
            1,
            CouncilNodeInfo::new("127.0.0.1:9001".parse().unwrap(), "node-1"),
        )]))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !councils[0].is_leader().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    councils[0]
        .add_learner(
            2,
            CouncilNodeInfo::new("127.0.0.1:9002".parse().unwrap(), "node-2"),
        )
        .await
        .unwrap();
    let app_id = AppId::new("api", "default");
    councils[0]
        .write(RaftRequest::AppSpec {
            app_id: app_id.clone(),
            spec: Box::new(
                Config::parse("[app.api]\nimage='old'\n")
                    .unwrap()
                    .app
                    .remove("api")
                    .unwrap(),
            ),
        })
        .await
        .unwrap();
    let old_leader = Harness::start_with(HarnessOptions {
        council: Some(councils[0].clone()),
        node_name: Some("node-1".into()),
        ..Default::default()
    })
    .await;
    let scratch = tempfile::tempdir().unwrap();
    let started = scratch.path().join("started");
    let attempts = scratch.path().join("attempts");
    let release = scratch.path().join("release");
    let replacement = scratch.path().join("replacement");
    let mut config = Config::parse("[app.api]\nimage='new'\n[job.migrate]\nruntime = \"process\"\nexec = \"/bin/sh\"\nrun_before=['app.api']\n").unwrap();
    config.job.get_mut("migrate").unwrap().command = Some(vec![
        "-c".into(),
        format!(
            "printf 'attempt\\n' >> '{}'; touch '{}'; while [ ! -e '{}' ]; do sleep 0.02; done; exit 0",
            attempts.display(),
            started.display(),
            release.display(),
        ),
    ]);
    let original_config = config.clone();
    let old_client = old_leader.client.clone();
    let mut old_apply = tokio::spawn(async move { old_client.apply(&original_config).await });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    councils[0]
        .change_membership(std::collections::BTreeSet::from([2]))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !councils[1].is_leader().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let new_leader = Harness::start_with(HarnessOptions {
        council: Some(councils[1].clone()),
        node_name: Some("node-2".into()),
        ..Default::default()
    })
    .await;
    if mode == "spec" {
        config.job.get_mut("migrate").unwrap().command = Some(vec![
            "-c".into(),
            format!("touch '{}'; exit 0", replacement.display()),
        ]);
    } else if mode == "ordinary-job" {
        config.app.clear();
        let job = config.job.get_mut("migrate").unwrap();
        job.run_before.clear();
        job.command = Some(vec![
            "-c".into(),
            format!("touch '{}'; exit 0", replacement.display()),
        ]);
    } else if mode == "ordinary-app" {
        config.job.clear();
        config.app.get_mut("api").unwrap().image = Some("competing".into());
    } else if mode == "target" {
        let mut app = config.app.remove("api").unwrap();
        app.image = Some("replacement".into());
        config.app.insert("other-api".into(), app);
        config.job.get_mut("migrate").unwrap().run_before = vec!["app.other-api".into()];
    }
    // The harness's leader loop takes chunks back from a holder silent for
    // three seconds (`with_timings` above), and node-1 never answers node-2.
    // A conservative hook's lost holder is an unknown outcome, so the same
    // operation first stays pending and then reports that outcome (#774).
    // Wait well past the silence so "same" checks the settled answer instead
    // of racing it; a conflicting claim must still refuse promptly.
    let window = if mode == "same" {
        Duration::from_secs(15)
    } else {
        Duration::from_secs(3)
    };
    let retry = tokio::time::timeout(window, new_leader.client.apply(&config)).await;
    // Release before assertions so even unchanged production's erroneous
    // second launch is cleaned up and the original process does not linger.
    std::fs::write(&release, b"release").unwrap();
    // The follower's old observer may remain pending: it cannot turn a
    // vanished owner into success. Cancel the observer, preserving the run.
    let stale_result = tokio::time::timeout(Duration::from_secs(3), &mut old_apply).await;
    if stale_result.is_err() {
        old_apply.abort();
    }
    let final_state = councils[1].desired_state().await;
    drop(new_leader);
    drop(old_leader);
    for council in councils {
        council.shutdown().await.unwrap();
    }
    if mode == "same" {
        // The repeat is the same durable operation, never a new attempt: it
        // reports the original hook's unknown ownership rather than success.
        let operations: Vec<String> = final_state
            .task_arrays
            .deployments()
            .map(|(id, _)| id.to_string())
            .collect();
        assert_eq!(operations.len(), 1, "{operations:?}");
        let error = retry
            .expect("the repeat must settle once node-1's silence makes its hook unknown")
            .expect_err("an uncertain prerequisite was reported as applied");
        let message = error.to_string();
        assert!(
            message.contains(&operations[0]) && message.contains("retains unknown hook ownership"),
            "an idempotent repeat must observe the same operation's unknown hook: {message}"
        );
    } else {
        let error = retry
            .expect("a conflicting claim must refuse promptly")
            .expect_err("a different operation stole the uncertain owner");
        assert!(error.to_string().contains("409"), "{mode}: {error}");
    }
    assert!(
        !matches!(stale_result, Ok(Ok(Ok(_)))),
        "old leader reported success"
    );
    assert_eq!(
        std::fs::read_to_string(attempts).unwrap().lines().count(),
        1
    );
    assert!(!replacement.exists());
    assert_eq!(final_state.apps[&app_id].image.as_deref(), Some("old"));
    assert!(
        !final_state
            .apps
            .contains_key(&AppId::new("other-api", "default"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_leader_cannot_repeat_an_uncertain_prerequisite_attempt() {
    assert_handover_keeps_an_uncertain_prerequisite_claim("same").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changing_a_prerequisite_spec_cannot_bypass_its_handover_claim() {
    assert_handover_keeps_an_uncertain_prerequisite_claim("spec").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changing_a_prerequisite_target_cannot_bypass_its_handover_claim() {
    assert_handover_keeps_an_uncertain_prerequisite_claim("target").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ordinary_job_apply_cannot_take_over_a_prerequisite_claim_after_handover() {
    assert_handover_keeps_an_uncertain_prerequisite_claim("ordinary-job").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ordinary_app_apply_cannot_bypass_its_prerequisite_claim_after_handover() {
    assert_handover_keeps_an_uncertain_prerequisite_claim("ordinary-app").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_cluster_prerequisites_are_refused_before_any_app_commit() {
    let council = single_node_leader().await;
    let harness = Harness::start_with(HarnessOptions {
        council: Some(council.clone()),
        ..Default::default()
    })
    .await;
    let http = reqwest::Client::new();
    for body in [
        "[job.migrate]\nruntime = \"process\"\nexec = \"/usr/bin/true\"\ncommand=[]\nrun_before=['app.absent']\n",
        "[app.web]\nimage='proc-grill:image-ignored'\nnamespace='production'\n[job.migrate]\nruntime = \"process\"\nexec = \"/usr/bin/true\"\ncommand=[]\nnamespace='other'\nrun_before=['app.web']\n",
        "[app.web]\nimage='proc-grill:image-ignored'\n[job.migrate]\nruntime = \"process\"\nexec = \"/usr/bin/true\"\ncommand=[]\nrun_before=['job.web']\n",
    ] {
        let response = http
            .post(format!("{}/v1/apply", harness.base_url))
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "accepted {body}"
        );
        let error = response.text().await.unwrap();
        assert!(error.contains("run_before"), "{error}");
        assert!(council.desired_state().await.apps.is_empty());
    }
    drop(harness);
    council.shutdown().await.unwrap();
}

struct PrerequisiteReleaseOnDrop(std::path::PathBuf);

impl Drop for PrerequisiteReleaseOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"release");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_running_cluster_migration_never_publishes_its_app_revision() {
    use reliaburger::bun::deploy_operations::{DeployOperationOutcome, DeployTargetKind};
    use reliaburger::council::types::RaftRequest;
    use reliaburger::meat::AppId;

    let council = single_node_leader().await;
    let app_id = AppId::new("api", "default");
    let old = Config::parse("[app.api]\nimage='old'\n")
        .unwrap()
        .app
        .remove("api")
        .unwrap();
    council
        .write(RaftRequest::AppSpec {
            app_id: app_id.clone(),
            spec: Box::new(old),
        })
        .await
        .unwrap();
    let harness = Harness::start_with(HarnessOptions {
        council: Some(council.clone()),
        ..Default::default()
    })
    .await;
    let scratch = tempfile::tempdir().unwrap();
    let started = scratch.path().join("started");
    let release = scratch.path().join("release");
    let finished = scratch.path().join("finished");
    let release_on_drop = PrerequisiteReleaseOnDrop(release.clone());
    let command = format!(
        "touch '{}'; while [ ! -e '{}' ]; do sleep 0.02; done; touch '{}'; exit 0",
        started.display(),
        release.display(),
        finished.display()
    );
    let mut config = Config::parse(
        "[app.api]\nimage='new'\n[job.migrate]\nruntime = \"process\"\nexec = \"/bin/sh\"\nrun_before=['app.api']\n",
    )
    .unwrap();
    config.job.get_mut("migrate").unwrap().command = Some(vec!["-c".into(), command]);
    let client = harness.client.clone();
    let apply = tokio::spawn(async move { client.apply(&config).await });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("actual ProcessGrill migration must acknowledge its blocked phase");

    let blocked = council.desired_state().await;
    assert_eq!(blocked.apps[&app_id].image.as_deref(), Some("old"));
    assert_eq!(blocked.task_arrays.deployments().count(), 1);
    let (claim_id, claim) = blocked.task_arrays.deployments().next().unwrap();
    let claim_id = claim_id.to_owned();
    assert!(!claim.apps_committed);
    assert!(claim.blocks("api", "default"));

    let snapshot = harness.client.deploy_operations().await.unwrap();
    assert_eq!(snapshot.active_deploys.len(), 1);
    let operation = &snapshot.active_deploys[0];
    assert!(operation.targets.iter().any(|target| {
        target.kind == DeployTargetKind::App
            && target.name == "api"
            && target.namespace == "default"
    }));
    assert!(operation.targets.iter().any(|target| {
        target.kind == DeployTargetKind::Job
            && target.name == "migrate"
            && target.namespace == "default"
    }));
    let deploy_id = operation.id.clone();
    assert_eq!(deploy_id.as_str(), claim_id);
    let cancellation = harness
        .client
        .cancel_deploy(deploy_id.as_str())
        .await
        .expect("the actual cancellation route must acknowledge the live operation");
    assert_eq!(cancellation.id, deploy_id);
    assert!(cancellation.cancellation_requested_at.is_some());
    assert!(cancellation.outcome.is_none());
    assert!(
        !finished.exists(),
        "migration gate opened before cancellation"
    );

    // A racing zero exit cannot erase cancellation. Cleanup may kill the process
    // before it sees this release; either way accepted cancellation wins.
    std::fs::write(&release, b"release").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let arrays = council.desired_state().await.task_arrays;
            let record = arrays
                .deployments()
                .find(|(id, _)| *id == claim_id)
                .unwrap()
                .1;
            if record.completed {
                break;
            }
            assert!(
                record.cancelled,
                "cancellation intent disappeared before cleanup"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelled owner did not positively settle");
    let result = tokio::time::timeout(Duration::from_secs(10), apply)
        .await
        .expect("cancelled apply must reach a bounded observable outcome")
        .unwrap();
    let after = council.desired_state().await;
    let snapshot = harness.client.deploy_operations().await.unwrap();
    let observed = snapshot
        .history
        .iter()
        .chain(snapshot.active_deploys.iter())
        .find(|operation| operation.id == deploy_id)
        .expect("cancellation cannot erase the operation's observable identity")
        .clone();
    // Release owned runtime/server resources before checking the frozen oracle.
    drop(release_on_drop);
    drop(harness);
    council.shutdown().await.unwrap();

    assert_eq!(
        after.apps[&app_id].image.as_deref(),
        Some("old"),
        "a cancelled migration published a new desired app after its zero exit"
    );
    let receipt = after
        .task_arrays
        .deployments()
        .find(|(id, _)| *id == claim_id)
        .unwrap()
        .1;
    assert!(receipt.cancelled && receipt.completed);
    assert!(!receipt.apps_committed);
    assert_eq!(
        receipt.outcome,
        Some(reliaburger::meat::job_deploy::DeploymentOutcome::Cancelled)
    );
    assert!(
        result.is_err(),
        "cancelled apply reported success: {result:?}"
    );
    assert_eq!(observed.id, deploy_id);
    assert!(observed.cancellation_requested_at.is_some());
    assert_ne!(observed.outcome, Some(DeployOperationOutcome::Completed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn common_run_logs_keep_the_logical_scope_and_select_the_stable_run() {
    let scratch = tempfile::tempdir().unwrap();
    let store = Arc::new(RwLock::new(reliaburger::ketchup::log_store::LogStore::new(
        scratch.path().to_path_buf(),
    )));
    let (sink, mut records) = mpsc::channel(16);
    let harness = Harness::start_with(HarnessOptions {
        log_sink: Some(sink),
        log_store: Some(store.clone()),
        ..Default::default()
    })
    .await;
    let jobs = jobs_from(
        "[job.migration]\nruntime = \"process\"\nexec = \"/bin/sh\"\ncommand=['-c','echo selected-output']\nnamespace='team'",
    );
    let response = harness.client.submit_batch(&jobs, None).await.unwrap();
    let run = response["executions"]["migration"].as_str().unwrap();
    let record = tokio::time::timeout(Duration::from_secs(10), records.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.instance, run);
    assert_eq!(record.app, "migration");
    assert_eq!(record.namespace, "team");
    store.write().await.ingest(&record);
    let http = reqwest::Client::new();
    let selected = http
        .get(format!(
            "{}/v1/logs/entries/migration/team?instance={run}",
            harness.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(selected.status(), 200);
    assert!(selected.text().await.unwrap().contains("selected-output"));
    let wrong_scope = http
        .get(format!(
            "{}/v1/logs/entries/other/team?instance={run}",
            harness.base_url
        ))
        .send()
        .await
        .unwrap();
    assert!(!wrong_scope.status().is_success());
}

/// Admission is durable queuing; current worker budgets govern physical starts.
/// Aggregator freshness belongs to app placement, not common job admission.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn common_jobs_execute_with_worker_evidence_independent_of_global_capacity_reports() {
    for mode in ["absent", "stale", "closed", "previous-term"] {
        let council = single_node_leader().await;
        let mut options = HarnessOptions {
            council: Some(council.clone()),
            node_name: Some("worker".into()),
            ..Default::default()
        };
        match mode {
            "stale" => {
                options.capacity_nodes = vec!["worker".into()];
                options.stale_capacity_nodes = vec!["worker".into()];
            }
            "closed" => options.close_capacity_channel = true,
            "previous-term" => {
                let (tx, rx) = tokio::sync::watch::channel(
                    reliaburger::reporting::aggregator::AggregatedState {
                        leadership_epoch: Some(council.current_term().saturating_sub(1)),
                        ..Default::default()
                    },
                );
                options.aggregated_override = Some(rx);
                drop(tx);
            }
            _ => {}
        }
        let harness = Harness::start_with(options).await;
        let response = harness
            .client
            .submit_batch(&jobs_from(
                "[job.worker-proof]\nruntime = \"process\"\nexec = \"/usr/bin/true\"\ncommand=[]",
            ), None)
            .await
            .unwrap();
        assert_eq!(response["assigned"], 1, "{mode}: {response}");
        let id = response["batch_id"].as_u64().unwrap();
        let summary = harness.wait_done(id, 10).await;
        assert_eq!(summary["succeeded"], 1, "{mode}: {summary}");
        let desired = council.desired_state().await;
        let child = desired.task_arrays.manifest(id).unwrap().cohorts[0].1;
        let record = desired.task_arrays.get(child).unwrap();
        assert_eq!(record.state.spec.count, 1);
        assert!(
            !desired
                .task_arrays
                .jobs()
                .run(child)
                .unwrap()
                .replay_unknown
        );
        assert!(
            record
                .state
                .accepted_grant(reliaburger::meat::task_array::ChunkId(0))
                .is_some()
        );
        drop(harness);
        council.shutdown().await.unwrap();
    }
}
