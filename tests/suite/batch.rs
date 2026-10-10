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
use reliaburger::meat::batch_tracker::{BatchJobRecord, BatchRecord, JobStatus, epoch_now_secs};
use reliaburger::relish::client::BunClient;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::task_harness;
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

struct Harness {
    client: BunClient,
    base_url: String,
    port: u16,
    cmd_tx: mpsc::Sender<reliaburger::bun::agent::AgentCommand>,
    _tasks: TestTasks,
    _capacity_publisher:
        Option<tokio::sync::watch::Sender<reliaburger::reporting::aggregator::AggregatedState>>,
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
            cmd_tx,
            _tasks: TestTasks::new(shutdown, vec![agent_task, server_task]),
            _capacity_publisher: (!options.close_capacity_channel).then_some(_aggregated_tx),
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

async fn assert_pruned_execution_cannot_be_claimed_elsewhere(mode: &str, kind: &str) {
    use reliaburger::council::types::RaftRequest;
    use reliaburger::meat::batch_tracker::TERMINAL_RETENTION_SECS;
    use reliaburger::meat::types::AppId;

    let router = InMemoryRaftRouter::new();
    let mut nodes = Vec::new();
    for id in 1..=2 {
        let node = Arc::new(
            CouncilNode::new(
                id,
                fast_config(),
                InMemoryRaftNetworkFactory::new(id, router.clone()),
                MemLogStore::new(),
                CouncilStateMachine::new(),
                None,
            )
            .await
            .unwrap(),
        );
        router.register(id, node.raft().clone()).await;
        nodes.push(node);
    }
    nodes[0]
        .initialize(BTreeMap::from([(
            1,
            CouncilNodeInfo::new("127.0.0.1:9001".parse().unwrap(), "first-worker"),
        )]))
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !nodes[0].is_leader().await {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    nodes[0]
        .add_learner(
            2,
            CouncilNodeInfo::new("127.0.0.1:9002".parse().unwrap(), "other-worker"),
        )
        .await
        .unwrap();

    let first = Harness::start_with(HarnessOptions {
        council: Some(nodes[0].clone()),
        node_name: Some("first-worker".into()),
        ..local_capacity_options(nodes[0].clone(), "first-worker")
    })
    .await;
    // This fence belongs to the remaining internal leased-execution protocol.
    // Seed its replicated records directly; public jobs now use indexed grants.
    let execution = "migration-legacy-execution".to_string();
    let submitted_at = epoch_now_secs();
    let record = BatchRecord {
        jobs: vec![BatchJobRecord {
            name: "migration".into(),
            execution_name: execution.clone(),
            namespace: "default".into(),
            spec_digest: "a".repeat(64),
            resources: reliaburger::meat::Resources::default(),
            node: Some(reliaburger::meat::NodeId::new("first-worker")),
            status: JobStatus::Pending,
        }],
        submitted_at_epoch_secs: submitted_at,
    };
    let response = write_admission_fixture(
        &nodes[0],
        RaftRequest::BatchRegister {
            expected_log_id: None,
            batch: record,
        },
    )
    .await
    .unwrap();
    let reliaburger::council::types::CouncilResponse::BatchRegistered { batch_id } = response
    else {
        panic!("{response:?}")
    };
    nodes[0]
        .write(RaftRequest::BatchJobUpdate {
            batch_id,
            job_name: execution.clone(),
            namespace: "default".into(),
            status: JobStatus::Completed,
            exit_code: Some(0),
        })
        .await
        .unwrap();
    let marker: BatchRecord = serde_json::from_value(serde_json::json!({
        "jobs": [{
            "name": "prune-marker",
            "execution_name": "prune-marker-execution",
            "resources":{"cpu_millicores":0,"memory_bytes":0,"gpus":0}, "spec_digest": "a".repeat(64),
            "namespace": "default",
            "node": null,
            "status": "Unschedulable"
        }],
        "submitted_at_epoch_secs": submitted_at + TERMINAL_RETENTION_SECS + 1
    }))
    .unwrap();
    write_admission_fixture(
        &nodes[0],
        RaftRequest::BatchRegister {
            expected_log_id: None,
            batch: marker,
        },
    )
    .await
    .unwrap();
    assert!(
        nodes[0]
            .desired_state()
            .await
            .batch_state
            .get(batch_id)
            .is_none()
    );
    drop(first);

    let restored_dir = tempfile::tempdir().unwrap();
    let council = match mode {
        "handover" => {
            nodes[0]
                .change_membership(std::collections::BTreeSet::from([2]))
                .await
                .unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !nodes[1].is_leader().await {
                assert!(tokio::time::Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            nodes[1].clone()
        }
        "recovery" => {
            let path = restored_dir.path().join("snapshot.redb");
            {
                let db = redb::Database::create(&path).unwrap();
                CouncilStateMachine::persist_recovered_snapshot(
                    &db,
                    nodes[0].desired_state().await,
                )
                .unwrap();
            }
            let sm =
                CouncilStateMachine::with_store(Arc::new(redb::Database::create(&path).unwrap()))
                    .unwrap();
            assert!(sm.recovered_bootstrap_pending().await);
            let restored_router = InMemoryRaftRouter::new();
            let node = Arc::new(
                CouncilNode::new(
                    3,
                    fast_config(),
                    InMemoryRaftNetworkFactory::new(3, restored_router.clone()),
                    MemLogStore::new(),
                    sm,
                    None,
                )
                .await
                .unwrap(),
            );
            restored_router.register(3, node.raft().clone()).await;
            node.initialize(BTreeMap::from([(
                3,
                CouncilNodeInfo::new("127.0.0.1:9003".parse().unwrap(), "other-worker"),
            )]))
            .await
            .unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !node.is_leader().await {
                assert!(tokio::time::Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            node
        }
        "other-worker" => nodes[0].clone(),
        _ => unreachable!(),
    };
    // This worker has never loaded the old runner's job checkpoint. Only the
    // replicated ownership fence can establish that the name is unavailable.
    let other = Harness::start_with(HarnessOptions {
        council: Some(council.clone()),
        node_name: Some("other-worker".into()),
        ..Default::default()
    })
    .await;
    let config = Config::parse(&format!(
        "[{kind}.{execution}]\nimage = \"proc-grill:image-ignored\"\ncommand = [\"true\"]"
    ))
    .unwrap();
    let error = other
        .client
        .apply(&config)
        .await
        .expect_err("a different worker took over a pruned batch execution's global identity");
    assert!(error.to_string().contains("409"), "{mode}: {error}");
    assert!(
        !council
            .desired_state()
            .await
            .apps
            .contains_key(&AppId::new(&execution, "default"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_worker_cannot_reuse_an_execution_after_terminal_tracker_pruning() {
    assert_pruned_execution_cannot_be_claimed_elsewhere("other-worker", "app").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovered_raft_ownership_refuses_cross_worker_reuse_of_a_pruned_execution() {
    assert_pruned_execution_cannot_be_claimed_elsewhere("recovery", "app").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_leader_refuses_cross_worker_reuse_of_a_pruned_execution() {
    assert_pruned_execution_cannot_be_claimed_elsewhere("handover", "app").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_worker_cannot_apply_an_ordinary_job_over_a_pruned_batch_execution() {
    assert_pruned_execution_cannot_be_claimed_elsewhere("other-worker", "job").await;
}

fn jobs_from(toml: &str) -> std::collections::BTreeMap<String, reliaburger::config::job::JobSpec> {
    Config::parse(toml).unwrap().job
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retired_worker_in_a_fresh_api_roster_never_receives_new_batch_work() {
    let council = single_node_leader().await;
    let membership_log_id = *council.metrics().borrow().membership_config.log_id();
    let response = council
        .write(reliaburger::council::RaftRequest::DecommissionNode {
            node_id: "retired-worker".into(),
            retired_by: "operator".into(),
            reason: "powered off".into(),
            retired_at_unix_ms: 1,
            membership_log_id,
        })
        .await
        .unwrap();
    assert!(
        matches!(
            response,
            reliaburger::council::CouncilResponse::NodeDecommissioned { .. }
        ),
        "{response:?}"
    );
    let (address, mut dispatches, _worker_tasks) = pending_capacity_worker().await;
    let mut options = remote_capacity_options(council.clone(), address);
    options.membership.as_mut().unwrap()[0].node_id =
        reliaburger::meat::NodeId::new("retired-worker");
    options.capacity_nodes = vec!["retired-worker".into()];
    let harness = Harness::start_with(options).await;
    let response = reqwest::Client::new().post(format!("{}/v1/batch",harness.base_url))
        .json(&serde_json::json!({"jobs":[{"name":"retired-target","spec":{"runtime":"process","exec":"/usr/bin/true","command":[]}}]}))
        .send().await.unwrap();
    let status = response.status();
    let records = council.desired_state().await.batch_state.batches.len();
    drop(harness);
    council.shutdown().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::ACCEPTED);
    assert_eq!(records, 0);
    assert!(
        dispatches.try_recv().is_err(),
        "a retired worker received legacy dispatch"
    );
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

async fn pending_capacity_worker() -> (
    std::net::SocketAddr,
    mpsc::Receiver<serde_json::Value>,
    TestTasks,
) {
    let (dispatch_tx, dispatches) = mpsc::channel(8);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            "/v1/batch/run",
            axum::routing::post(
                move |headers: axum::http::HeaderMap,
                      axum::Json(dispatch): axum::Json<serde_json::Value>| {
                    let dispatch_tx = dispatch_tx.clone();
                    async move {
                        assert_eq!(
                            headers.get("authorization").unwrap().to_str().unwrap(),
                            format!("Bearer {TEST_SERVICE_TOKEN}")
                        );
                        dispatch_tx.send(dispatch).await.unwrap();
                        axum::http::StatusCode::ACCEPTED
                    }
                },
            ),
        )
        .route(
            "/v1/status",
            axum::routing::get(|| async { axum::Json(Vec::<serde_json::Value>::new()) }),
        );
    let shutdown = CancellationToken::new();
    let server_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(server_shutdown.cancelled_owned())
            .await
            .unwrap();
    });
    (address, dispatches, TestTasks::new(shutdown, vec![task]))
}

fn remote_capacity_options(
    council: Arc<CouncilNode>,
    address: std::net::SocketAddr,
) -> HarnessOptions {
    let mut options = capacity_options(council);
    options.node_name = Some("leader".into());
    options.membership.as_mut().unwrap()[0].address = address;
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

/// Losing a runner after the observation deadline doesn't prove that its
/// queued or running work has exited. The replicated record must retain it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_batch_with_an_unknown_runner_remains_nonterminal() {
    let council = single_node_leader().await;
    let record = BatchRecord {
        submitted_at_epoch_secs: 0,
        jobs: vec![BatchJobRecord {
            resources: reliaburger::meat::Resources::default(),
            name: "uncertain".into(),
            execution_name: "uncertain-execution".into(),
            spec_digest: "a".repeat(64),
            namespace: "default".into(),
            node: Some(reliaburger::meat::NodeId::new("unreachable")),
            status: JobStatus::Pending,
        }],
    };
    write_admission_fixture(
        &council,
        reliaburger::council::types::RaftRequest::BatchRegister {
            expected_log_id: None,
            batch: record,
        },
    )
    .await
    .unwrap();
    let harness = Harness::start_with(capacity_options(council.clone())).await;
    harness.client.batch_status(1).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let observed = loop {
        let observed = council
            .desired_state()
            .await
            .batch_state
            .get(1)
            .unwrap()
            .clone();
        assert!(
            !observed.is_terminal(),
            "unknown execution was retired: {observed:?}"
        );
        if tokio::time::Instant::now() >= deadline {
            break observed;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    drop(harness);
    council.shutdown().await.unwrap();
    assert!(
        !observed.is_terminal(),
        "unknown execution was retired: {observed:?}"
    );
}

async fn assert_batch_refuses_declarative_job_field(internal: bool, field: &str) {
    let council = single_node_leader().await;
    let scratch = tempfile::tempdir().unwrap();
    let launches = scratch.path().join("launches");
    let records_dir = scratch.path().join("records");
    let runner = Harness::start_with(HarnessOptions {
        council: (!internal).then(|| council.clone()),
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
    let mut request = serde_json::json!({
        "jobs": [{"name": "migration", "namespace": "default", "spec": spec}]
    });
    if internal {
        request["batch_id"] = serde_json::json!(99);
        request["execution_labels"] = serde_json::json!({
            "migration": {"name": "migration", "namespace": "default"}
        });
    }
    let route = if internal { "batch/run" } else { "batch" };
    let response = reqwest::Client::new()
        .post(format!("{}/v1/{route}", runner.base_url))
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
    assert!(council.desired_state().await.batch_state.batches.is_empty());
    assert!(!records_dir.join("job-attempts.checkpoint").exists());
    assert!(!launches.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_batch_refuses_a_cron_schedule_before_registration() {
    assert_batch_refuses_declarative_job_field(false, "schedule").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_batch_refuses_run_before_without_an_app_submission() {
    assert_batch_refuses_declarative_job_field(false, "run_before").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn internal_batch_refuses_a_cron_schedule_before_ownership() {
    assert_batch_refuses_declarative_job_field(true, "schedule").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn internal_batch_refuses_run_before_without_an_app_submission() {
    assert_batch_refuses_declarative_job_field(true, "run_before").await;
}

/// A delayed dispatch during automatic retry must observe the current attempt,
/// not the failed predecessor, and a later replay must retain the final outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_batch_retry_keeps_current_attempt_outcome_through_recovery() {
    let scratch = tempfile::tempdir().unwrap();
    let launches = scratch.path().join("launches");
    let release = scratch.path().join("release");
    let records_dir = scratch.path().join("records");
    let (report_tx, mut reports) = mpsc::channel(8);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let callback_url = format!("http://{}", listener.local_addr().unwrap());
    let callback = axum::Router::new().route(
        "/v1/batch/{id}/report",
        axum::routing::post(move |axum::Json(report): axum::Json<serde_json::Value>| {
            let report_tx = report_tx.clone();
            async move {
                let _ = report_tx.send(report).await;
                axum::http::StatusCode::OK
            }
        }),
    );
    let shutdown = CancellationToken::new();
    let callback_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, callback)
            .with_graceful_shutdown(callback_shutdown.cancelled_owned())
            .await
            .unwrap();
    });
    let _callback_tasks = TestTasks::new(shutdown, vec![task]);
    let mut runner = Harness::start_with(HarnessOptions {
        records_dir: Some(records_dir.clone()),
        ..Default::default()
    })
    .await;
    let execution = "batch-22222222222222222222222222222222";
    let command = format!(
        "if [ ! -f '{}' ]; then printf 'attempt\\n' >> '{}'; exit 1; fi; \
         printf 'attempt\\n' >> '{}'; while [ ! -f '{}' ]; do sleep 0.01; done; exit 0",
        launches.display(),
        launches.display(),
        launches.display(),
        release.display()
    );
    let request = serde_json::json!({
        "batch_id": 99, "callback_base_url": callback_url,
        "jobs": [{"name": execution, "namespace": "team", "spec": {
            "runtime":"process","exec":"/bin/sh", "command": ["-c", command],
            "namespace": "team"
        }}],
        "execution_labels": {(execution): {"name": "migration", "namespace": "team"}}
    });
    let http = reqwest::Client::new();
    let first = http
        .post(format!("{}/v1/batch/run", runner.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), reqwest::StatusCode::ACCEPTED);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if std::fs::read_to_string(&launches).unwrap_or_default() == "attempt\nattempt\n" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let retry = http
        .post(format!("{}/v1/batch/run", runner.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&request)
        .send()
        .await
        .unwrap();
    let premature = tokio::time::timeout(Duration::from_millis(150), reports.recv()).await;
    // Release the actual child before assertions so failures cannot leave it blocked.
    std::fs::write(&release, b"release").unwrap();
    assert_eq!(retry.status(), reqwest::StatusCode::ACCEPTED);
    assert!(
        premature.is_err(),
        "previous retry outcome was treated as terminal: {premature:?}"
    );
    for _ in 0..2 {
        let report = tokio::time::timeout(Duration::from_secs(10), reports.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report["job_name"], execution);
        assert_eq!(report["status"], "completed", "{report}");
        assert_eq!(report["exit_code"], 0, "{report}");
    }
    drop(runner);
    runner = Harness::start_with(HarnessOptions {
        records_dir: Some(records_dir),
        ..Default::default()
    })
    .await;
    let recovered = http
        .post(format!("{}/v1/batch/run", runner.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&request)
        .send()
        .await
        .unwrap();
    let recovered_status = recovered.status();
    let recovered_body = recovered.text().await.unwrap();
    assert_eq!(
        recovered_status,
        reqwest::StatusCode::ACCEPTED,
        "{recovered_body}"
    );
    let report = tokio::time::timeout(Duration::from_secs(10), reports.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report["status"], "completed", "{report}");
    assert_eq!(report["exit_code"], 0, "{report}");
    assert_eq!(
        std::fs::read_to_string(&launches).unwrap(),
        "attempt\nattempt\n"
    );
}

async fn assert_batch_dispatch_is_idempotent(recover: bool, mismatch: Option<&str>) {
    let scratch = tempfile::tempdir().unwrap();
    let launches = scratch.path().join("launches");
    let records_dir = scratch.path().join("records");
    let (report_tx, mut reports) = mpsc::channel(8);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let callback_url = format!("http://{}", listener.local_addr().unwrap());
    let callback = axum::Router::new().route(
        "/v1/batch/{id}/report",
        axum::routing::post(move |axum::Json(report): axum::Json<serde_json::Value>| {
            let report_tx = report_tx.clone();
            async move {
                report_tx.send(report).await.unwrap();
                axum::http::StatusCode::OK
            }
        }),
    );
    let shutdown = CancellationToken::new();
    let callback_shutdown = shutdown.clone();
    let callback_task = tokio::spawn(async move {
        axum::serve(listener, callback)
            .with_graceful_shutdown(callback_shutdown.cancelled_owned())
            .await
            .unwrap();
    });
    let _callback_tasks = TestTasks::new(shutdown, vec![callback_task]);
    let mut runner = Harness::start_with(HarnessOptions {
        records_dir: Some(records_dir.clone()),
        ..Default::default()
    })
    .await;
    let execution = "batch-11111111111111111111111111111111";
    let command = format!("printf 'launch\\n' >> '{}'", launches.display());
    let retiring = matches!(
        mismatch,
        Some("retire" | "retire-batch" | "retire-fault" | "retire-collision")
    );
    let image = if retiring {
        format!("proc-grill:{}", "x".repeat(1024 * 1024))
    } else {
        "proc-grill:image-ignored".into()
    };
    let mut request = serde_json::json!({
        "batch_id": 99, "callback_base_url": callback_url,
        "jobs": [{"name": execution, "namespace": "team", "spec": {
            "image": image, "command": ["sh", "-c", command], "namespace": "team"
        }}],
        "execution_labels": {(execution): {"name": "migration", "namespace": "team"}}
    });
    let http = reqwest::Client::new();
    for attempt in 0..2 {
        if attempt == 1 && retiring {
            let checkpoint = records_dir.join("job-attempts.checkpoint");
            let original_bytes = std::fs::metadata(&checkpoint).unwrap().len();
            let original_inventory: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
            let backup = records_dir.join("original-job-attempts.checkpoint");
            if mismatch == Some("retire-fault") {
                std::fs::rename(&checkpoint, &backup).unwrap();
                std::fs::create_dir(&checkpoint).unwrap();
            }
            let (response, retired) = tokio::sync::oneshot::channel();
            runner
                .cmd_tx
                .send(reliaburger::bun::agent::AgentCommand::Retire {
                    app_name: execution.into(),
                    namespace: "team".into(),
                    response,
                })
                .await
                .unwrap();
            let result = tokio::time::timeout(Duration::from_secs(10), retired)
                .await
                .unwrap()
                .unwrap();
            if mismatch == Some("retire-fault") {
                assert!(
                    result.is_err(),
                    "retirement acknowledged an uncertain proof publication"
                );
                std::fs::remove_dir(&checkpoint).unwrap();
                std::fs::rename(&backup, &checkpoint).unwrap();
                let retry = http
                    .post(format!("{}/v1/batch/run", runner.base_url))
                    .bearer_auth(TEST_SERVICE_TOKEN)
                    .json(&request)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(retry.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(std::fs::read_to_string(&launches).unwrap(), "launch\n");
            } else {
                result.unwrap();
                assert!(
                    std::fs::metadata(&checkpoint).unwrap().len() < original_bytes / 2,
                    "positive retirement must compact the specification while retaining replay proof"
                );
            }
            if mismatch == Some("retire-collision") {
                let mut inventory: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
                assert_eq!(
                    inventory["retired_batch_executions"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
                inventory["jobs"]
                    .as_array_mut()
                    .unwrap()
                    .push(original_inventory["jobs"][0].clone());
                drop(runner);
                std::fs::write(&checkpoint, serde_json::to_vec(&inventory).unwrap()).unwrap();
                let (_commands, commands) = mpsc::channel(8);
                let mut recovering = BunAgent::new(
                    ProcessGrill::new(),
                    PortAllocator::new(42000, 43000),
                    commands,
                    CancellationToken::new(),
                );
                recovering.set_records_dir(records_dir.clone());
                assert!(
                    recovering.adopt_recorded_instances().await.is_err(),
                    "recovery accepted active and retired owners for one execution identity"
                );
                break;
            }
        }
        if attempt == 1 && recover {
            drop(runner);
            runner = Harness::start_with(HarnessOptions {
                records_dir: Some(records_dir.clone()),
                ..Default::default()
            })
            .await;
        }
        if attempt == 1 && matches!(mismatch, Some("public" | "public-rerun" | "public-app")) {
            let mut config = Config {
                job: std::collections::BTreeMap::from([(
                    execution.to_string(),
                    serde_json::from_value(request["jobs"][0]["spec"].clone()).unwrap(),
                )]),
                ..Config::default()
            };
            if mismatch == Some("public-app") {
                let job = config.job.remove(execution).unwrap();
                let mut app = Config::parse(&format!(
                    "[app.{execution}]\nimage='proc-grill:image-ignored'\nnamespace='team'\n"
                ))
                .unwrap()
                .app
                .remove(execution)
                .unwrap();
                app.command = job.command.unwrap();
                config.app.insert(execution.into(), app);
            }
            let outcome = if mismatch == Some("public-rerun") {
                runner.client.apply_rerunning_jobs(&config).await
            } else {
                runner.client.apply(&config).await
            };
            assert!(
                outcome.is_err(),
                "public apply took over a batch execution: {outcome:?}"
            );
            break;
        }
        if attempt == 1 {
            match mismatch {
                Some("spec") => {
                    request["jobs"][0]["spec"]["command"] = serde_json::json!([
                        "sh",
                        "-c",
                        format!("printf 'changed\\n' >> '{}'", launches.display())
                    ])
                }
                Some("label") => {
                    request["execution_labels"][execution]["name"] =
                        serde_json::json!("other-migration")
                }
                Some("batch" | "retire-batch") => {
                    request["batch_id"] = serde_json::json!(100);
                }
                None | Some("retire" | "retire-fault") => {}
                Some(other) => panic!("unknown mismatch {other}"),
            }
        }
        let response = http
            .post(format!("{}/v1/batch/run", runner.base_url))
            .bearer_auth(TEST_SERVICE_TOKEN)
            .json(&request)
            .send()
            .await
            .unwrap();
        if attempt == 1 && matches!(mismatch, Some("spec" | "label" | "batch" | "retire-batch")) {
            assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
            break;
        }
        if attempt == 1
            && mismatch == Some("retire")
            && matches!(
                response.status(),
                reqwest::StatusCode::CONFLICT | reqwest::StatusCode::GONE
            )
        {
            break;
        }
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
        let report = tokio::time::timeout(Duration::from_secs(10), reports.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report["job_name"], execution);
        assert_eq!(report["status"], "completed", "{report}");
        if attempt == 1 && mismatch == Some("retire") {
            break;
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let rows: Vec<serde_json::Value> = http
                    .get(format!("{}/v1/jobs", runner.base_url))
                    .bearer_auth(TEST_SERVICE_TOKEN)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if rows.iter().any(|row| row["state"] == "stopped") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(&launches).unwrap(),
        "launch\n",
        "an identical service dispatch launched the same execution twice"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_internal_batch_dispatch_preserves_a_successful_attempt() {
    assert_batch_dispatch_is_idempotent(false, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovered_batch_execution_refuses_to_launch_again_for_an_identical_dispatch() {
    assert_batch_dispatch_is_idempotent(true, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retirement_keeps_a_batch_execution_fenced_against_a_delayed_retry() {
    assert_batch_dispatch_is_idempotent(false, Some("retire")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_preserves_a_retired_batchs_replay_fence() {
    assert_batch_dispatch_is_idempotent(true, Some("retire")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_existing_batch_execution_refuses_a_changed_spec_label_or_batch() {
    for mismatch in ["spec", "label", "batch"] {
        assert_batch_dispatch_is_idempotent(false, Some(mismatch)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recovered_batch_execution_refuses_a_changed_spec_label_or_batch() {
    for mismatch in ["spec", "label", "batch"] {
        assert_batch_dispatch_is_idempotent(true, Some(mismatch)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retired_batch_execution_cannot_lend_its_result_to_another_batch() {
    for recover in [false, true] {
        assert_batch_dispatch_is_idempotent(recover, Some("retire-batch")).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uncertain_retirement_proof_keeps_the_original_attempt_fenced_through_recovery() {
    assert_batch_dispatch_is_idempotent(true, Some("retire-fault")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_refuses_an_active_and_retired_owner_for_the_same_batch_execution() {
    assert_batch_dispatch_is_idempotent(false, Some("retire-collision")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retiring_an_unknown_batch_exit_does_not_fabricate_a_terminal_retry_result() {
    use sha2::{Digest, Sha256};

    let directory = tempfile::tempdir().unwrap();
    let records = directory.path().join("records");
    std::fs::create_dir(&records).unwrap();
    let execution = "unknown-retired-execution";
    let launches = directory.path().join("launches");
    let spec: reliaburger::config::job::JobSpec = serde_json::from_value(serde_json::json!({
        "image": "proc-grill:image-ignored",
        "command": ["sh", "-c", format!("touch '{}'", launches.display())],
        "namespace": "team"
    }))
    .unwrap();
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&("team", "migration", &spec)).unwrap())
    );
    // Positive absence and unknown exit are distinct evidence. Recovery must
    // keep that distinction when an operator explicitly retires the identity.
    std::fs::write(
        records.join("job-attempts.checkpoint"),
        serde_json::to_vec(&serde_json::json!({
            "schema": 3,
            "jobs": [{
                "name": execution, "namespace": "team", "spec": spec,
                "runtime": "Process", "generation": 1, "restart_count": 0,
                "phase": "Unknown", "runtime_absent": true,
                "batch_execution": {
                    "batch_id": 99, "logical_name": "migration", "spec_digest": digest,
                    "observed_exit_code": null
                }
            }],
            "retired_batch_executions": []
        }))
        .unwrap(),
    )
    .unwrap();
    let mut runner = Harness::start_with(HarnessOptions {
        records_dir: Some(records.clone()),
        ..Default::default()
    })
    .await;
    let (response, retired) = tokio::sync::oneshot::channel();
    runner
        .cmd_tx
        .send(reliaburger::bun::agent::AgentCommand::Retire {
            app_name: execution.into(),
            namespace: "team".into(),
            response,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), retired)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let inventory: serde_json::Value =
        serde_json::from_slice(&std::fs::read(records.join("job-attempts.checkpoint")).unwrap())
            .unwrap();
    assert!(inventory["jobs"].as_array().unwrap().is_empty());
    let proofs = inventory["retired_batch_executions"].as_array().unwrap();
    assert_eq!(proofs.len(), 1);
    assert!(proofs[0]["batch_execution"]["observed_exit_code"].is_null());
    for recover in [false, true] {
        if recover {
            drop(runner);
            runner = Harness::start_with(HarnessOptions {
                records_dir: Some(records.clone()),
                ..Default::default()
            })
            .await;
        }
        let response = reqwest::Client::new()
            .post(format!("{}/v1/batch/run", runner.base_url))
            .bearer_auth(TEST_SERVICE_TOKEN)
            .json(&serde_json::json!({
                "batch_id": 99, "callback_base_url": null,
                "jobs": [{"name": execution, "namespace": "team", "spec": spec}],
                "execution_labels": {(execution): {"name": "migration", "namespace": "team"}}
            }))
            .send()
            .await
            .unwrap();
        assert!(
            matches!(
                response.status(),
                reqwest::StatusCode::CONFLICT | reqwest::StatusCode::SERVICE_UNAVAILABLE
            ),
            "unknown retired exit was acknowledged as an executable/completed retry: {response:?}"
        );
        assert!(!launches.exists());
    }
}

async fn assert_clustered_runner_cannot_create_from_stale_or_foreign_ownership(mode: &str) {
    use reliaburger::council::types::RaftRequest;
    use reliaburger::meat::batch_tracker::TERMINAL_RETENTION_SECS;
    use sha2::{Digest, Sha256};

    let council = single_node_leader().await;
    let execution = "indexed-past-execution";
    let directory = tempfile::tempdir().unwrap();
    let launches = directory.path().join("launches");
    let changed_launches = directory.path().join("changed-launches");
    let mut spec: reliaburger::config::job::JobSpec = serde_json::from_value(serde_json::json!({
        "image": "proc-grill:image-ignored", "command": ["sh", "-c", format!("touch '{}'", launches.display())],
        "namespace": "default"
    })).unwrap();
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&("default", execution, &spec)).unwrap())
    );
    let record: BatchRecord = serde_json::from_value(serde_json::json!({
        "jobs": [{
            "name": execution, "execution_name": execution, "namespace": "default",
            "node": if mode == "wrong-node" { "other-worker" } else { "worker" }, "status": "Pending",
            "resources":{"cpu_millicores":0,"memory_bytes":0,"gpus":0}, "spec_digest": digest
        }],
        "submitted_at_epoch_secs": 1_000_000
    })).unwrap();
    write_admission_fixture(
        &council,
        RaftRequest::BatchRegister {
            expected_log_id: None,
            batch: record,
        },
    )
    .await
    .unwrap();
    if mode == "pruned" {
        council
            .write(RaftRequest::BatchJobUpdate {
                batch_id: 1,
                job_name: execution.into(),
                namespace: "default".into(),
                exit_code: Some(0),
                status: JobStatus::Completed,
            })
            .await
            .unwrap();
        let marker: BatchRecord = serde_json::from_value(serde_json::json!({
            "jobs": [{
                "name": "prune-marker", "execution_name": "prune-marker-execution",
                "resources":{"cpu_millicores":0,"memory_bytes":0,"gpus":0}, "spec_digest": "a".repeat(64),
                "namespace": "default", "node": null, "status": "Unschedulable"
            }],
            "submitted_at_epoch_secs": 1_000_000 + TERMINAL_RETENTION_SECS + 1
        }))
        .unwrap();
        write_admission_fixture(
            &council,
            RaftRequest::BatchRegister {
                expected_log_id: None,
                batch: marker,
            },
        )
        .await
        .unwrap();
        assert!(council.desired_state().await.batch_state.get(1).is_none());
    }
    if mode == "retired" {
        let membership_log_id = *council.metrics().borrow().membership_config.log_id();
        let response = council
            .write(RaftRequest::DecommissionNode {
                node_id: "worker".into(),
                retired_by: "operator".into(),
                reason: "retired after allocation".into(),
                retired_at_unix_ms: 1,
                membership_log_id,
            })
            .await
            .unwrap();
        assert!(
            matches!(
                response,
                reliaburger::council::CouncilResponse::NodeDecommissioned { .. }
            ),
            "{response:?}"
        );
    }
    if mode == "altered-spec" {
        spec.command = Some(vec![
            "-c".into(),
            format!("touch '{}'", changed_launches.display()),
        ]);
    }
    let records = directory.path().join("records");
    let runner = Harness::start_with(HarnessOptions {
        council: Some(council.clone()),
        node_name: Some("worker".into()),
        records_dir: Some(records.clone()),
        ..Default::default()
    })
    .await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/batch/run", runner.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&serde_json::json!({
            "batch_id": 1, "callback_base_url": null,
            "jobs": [{"name": execution, "namespace": "default", "spec": spec}],
            "execution_labels": {(execution): {"name": execution, "namespace": "default"}}
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let jobs: Vec<serde_json::Value> = reqwest::Client::new()
        .get(format!("{}/v1/jobs", runner.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    drop(runner);
    council.shutdown().await.unwrap();
    if mode == "altered-spec" {
        assert_eq!(
            status,
            reqwest::StatusCode::CONFLICT,
            "a worker admitted a changed spec before recording original ownership"
        );
    } else {
        assert!(
            matches!(
                status,
                reqwest::StatusCode::CONFLICT | reqwest::StatusCode::SERVICE_UNAVAILABLE
            ),
            "a fresh runner created from {mode} ownership: {status}"
        );
    }
    assert!(
        jobs.is_empty(),
        "refusal left local admitted ownership: {jobs:?}"
    );
    assert!(!records.join("job-attempts.checkpoint").exists());
    assert!(!launches.exists());
    assert!(!changed_launches.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_global_replay_fence_without_a_live_assignment_cannot_authorise_a_new_runner() {
    assert_clustered_runner_cannot_create_from_stale_or_foreign_ownership("pruned").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_batch_assigned_to_another_worker_cannot_authorise_local_creation() {
    assert_clustered_runner_cannot_create_from_stale_or_foreign_ownership("wrong-node").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_batchs_original_spec_cannot_change_before_the_first_runner_admission() {
    assert_clustered_runner_cannot_create_from_stale_or_foreign_ownership("altered-spec").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delayed_first_batch_dispatch_cannot_launch_after_durable_node_retirement() {
    assert_clustered_runner_cannot_create_from_stale_or_foreign_ownership("retired").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn internal_batch_dispatch_cannot_claim_an_ordinary_execution_identity() {
    let scratch = tempfile::tempdir().unwrap();
    let launches = scratch.path().join("launches");
    let runner = Harness::start_with(HarnessOptions {
        records_dir: Some(scratch.path().join("records")),
        ..Default::default()
    })
    .await;
    // A batch-looking name remains ordinary when admitted through public apply.
    let execution = "batch-ordinary-execution";
    let mut config = Config::parse(&format!(
        "[job.{execution}]\nruntime = \"process\"\nexec = \"/bin/sh\"\n"
    ))
    .unwrap();
    config.job.get_mut(execution).unwrap().command = Some(vec![
        "-c".into(),
        format!("printf 'launch\\n' >> '{}'", launches.display()),
    ]);
    runner.client.apply(&config).await.unwrap();
    let http = reqwest::Client::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let rows: Vec<serde_json::Value> = http
                .get(format!("{}/v1/jobs", runner.base_url))
                .bearer_auth(TEST_SERVICE_TOKEN)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if rows.iter().any(|row| row["state"] == "stopped") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let response = http
        .post(format!("{}/v1/batch/run", runner.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&serde_json::json!({
            "batch_id": 99,
            "jobs": [{"name": execution, "spec": config.job[execution]}],
            "execution_labels": {(execution): {"name": execution, "namespace": "default"}}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(std::fs::read_to_string(launches).unwrap(), "launch\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_apply_cannot_take_over_a_completed_batch_execution() {
    for path in ["public", "public-rerun", "public-app"] {
        assert_batch_dispatch_is_idempotent(false, Some(path)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_apply_cannot_take_over_a_recovered_batch_execution() {
    for path in ["public", "public-rerun", "public-app"] {
        assert_batch_dispatch_is_idempotent(true, Some(path)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_batch_checkpoint_refuses_new_work_without_fencing_existing_retries() {
    let scratch = tempfile::tempdir().unwrap();
    let records_dir = scratch.path().join("records");
    let (report_tx, mut reports) = mpsc::channel(32);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let callback_url = format!("http://{}", listener.local_addr().unwrap());
    let callback = axum::Router::new().route(
        "/v1/batch/{id}/report",
        axum::routing::post(move |axum::Json(report): axum::Json<serde_json::Value>| {
            let report_tx = report_tx.clone();
            async move {
                report_tx.send(report).await.unwrap();
                axum::http::StatusCode::OK
            }
        }),
    );
    let shutdown = CancellationToken::new();
    let callback_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, callback)
            .with_graceful_shutdown(callback_shutdown.cancelled_owned())
            .await
            .unwrap();
    });
    let _callback_tasks = TestTasks::new(shutdown, vec![task]);
    let mut runner = Harness::start_with(HarnessOptions {
        records_dir: Some(records_dir.clone()),
        ..Default::default()
    })
    .await;
    let http = reqwest::Client::new();
    // ProcessGrill ignores image bytes. Near-limit valid requests reach the
    // actual 16 MiB checkpoint bound with fewer whole-inventory publications,
    // while keeping each JSON request below the production 2 MiB body limit.
    let image = format!("proc-grill:{}", "x".repeat(15 * 128 * 1024));
    let mut first = None;
    let mut accepted = 0;
    let mut refused = false;
    for index in 0..20 {
        let execution = format!("batch-checkpoint-{index}");
        let request = serde_json::json!({
            "batch_id": index + 1, "callback_base_url": callback_url,
            "jobs": [{"name": execution, "spec": {
                "image": image, "command": ["true"]
            }}],
            "execution_labels": {(execution.clone()): {
                "name": format!("migration-{index}"), "namespace": "default"
            }}
        });
        assert!(
            serde_json::to_vec(&request).unwrap().len() < 2 * 1024 * 1024,
            "capacity fixture must pass the production request-body bound"
        );
        let response = http
            .post(format!("{}/v1/batch/run", runner.base_url))
            .bearer_auth(TEST_SERVICE_TOKEN)
            .json(&request)
            .send()
            .await
            .unwrap();
        if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            let error = response.text().await.unwrap();
            assert!(
                error.contains("checkpoint") || error.contains("capacity"),
                "a full checkpoint must explain why admission was refused: {error}"
            );
            refused = true;
            break;
        }
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
        let started = std::time::Instant::now();
        let report = tokio::time::timeout(Duration::from_secs(10), reports.recv())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "{execution} sent no report within 10 s after {accepted} accepted \
                     executions; a checkpoint publication past its bound leaves the outcome \
                     uncertain (see Bun's stderr above)"
                )
            })
            .unwrap();
        eprintln!(
            "batch-checkpoint fixture {execution} reported in {:?}",
            started.elapsed()
        );
        assert_eq!(report["job_name"], execution);
        assert_eq!(
            report["status"], "completed",
            "HTTP acceptance must reserve enough checkpoint space for terminal evidence: {report}"
        );
        first.get_or_insert(request);
        accepted += 1;
    }
    assert!(
        accepted >= 2,
        "the checkpoint must admit ordinary valid work"
    );
    assert!(
        refused,
        "the durable inventory must remain within its size bound"
    );
    assert!(
        std::fs::metadata(records_dir.join("job-attempts.checkpoint"))
            .unwrap()
            .len()
            <= 16 * 1024 * 1024
    );
    eprintln!(
        "batch-checkpoint fixture accepted={accepted} checkpoint-bytes={}",
        std::fs::metadata(records_dir.join("job-attempts.checkpoint"))
            .unwrap()
            .len()
    );
    for recover in [false, true] {
        if recover {
            drop(runner);
            runner = Harness::start_with(HarnessOptions {
                records_dir: Some(records_dir.clone()),
                ..Default::default()
            })
            .await;
        }
        let response = http
            .post(format!("{}/v1/batch/run", runner.base_url))
            .bearer_auth(TEST_SERVICE_TOKEN)
            .json(first.as_ref().unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::ACCEPTED,
            "predictable capacity refusal must not fence already-admitted executions"
        );
        let report = tokio::time::timeout(Duration::from_secs(10), reports.recv())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "the replayed first execution sent no report within 10 s (recovered: {recover})"
                )
            })
            .unwrap();
        assert_eq!(report["job_name"], "batch-checkpoint-0");
        assert_eq!(report["status"], "completed");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uncertain_batch_checkpoint_refuses_dispatch_and_keeps_retries_fenced() {
    let scratch = tempfile::tempdir().unwrap();
    let records_dir = scratch.path().join("records");
    let launches = scratch.path().join("launches");
    let runner = Harness::start_with(HarnessOptions {
        records_dir: Some(records_dir.clone()),
        ..Default::default()
    })
    .await;
    // Publication cannot replace a directory with the checkpoint file.
    let checkpoint = records_dir.join("job-attempts.checkpoint");
    std::fs::create_dir_all(&checkpoint).unwrap();
    let one = "batch-11111111111111111111111111111111";
    let two = "batch-22222222222222222222222222222222";
    let command = format!("printf 'launch\\n' >> '{}'", launches.display());
    let request = serde_json::json!({
        "batch_id": 99,
        "jobs": ([one, two].map(|execution| serde_json::json!({
            "name": execution, "namespace": "team", "spec": {
                "runtime":"process","exec":"/bin/sh", "command": ["-c", command], "namespace": "team"
            }
        }))),
        "execution_labels": {
            (one): {"name": "migration-one", "namespace": "team"},
            (two): {"name": "migration-two", "namespace": "team"}
        }
    });
    let http = reqwest::Client::new();
    for attempt in 0..2 {
        if attempt == 1 {
            std::fs::remove_dir(&checkpoint).unwrap();
        }
        let response = http
            .post(format!("{}/v1/batch/run", runner.base_url))
            .bearer_auth(TEST_SERVICE_TOKEN)
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        let error = response.text().await.unwrap();
        assert!(
            error.contains("checkpoint") || error.contains("uncertain"),
            "{error}"
        );
        assert!(
            !launches.exists(),
            "a worker ran before atomic admission was durable"
        );
        let rows: Vec<serde_json::Value> = http
            .get(format!("{}/v1/jobs", runner.base_url))
            .bearer_auth(TEST_SERVICE_TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            2,
            "admitted unknown identities vanished: {rows:?}"
        );
        for row in &rows {
            assert_eq!(row["state"], "unknown", "{row}");
            assert_eq!(row["namespace"], "team", "{row}");
            assert!(
                matches!(
                    row["name"].as_str(),
                    Some("migration-one" | "migration-two")
                ),
                "{row}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn predictable_host_policy_refusal_keeps_the_whole_batch_unadmitted() {
    let mut invalid =
        Config::parse("[job.invalid]\nnamespace='team'\nruntime='process'\nexec='/unapproved/echo'\ncommand=['rejected']\n")
            .unwrap();
    predictable_policy_refusal_keeps_batch_unadmitted(invalid.job.remove("invalid").unwrap()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn predictable_process_limits_refusal_keeps_the_whole_batch_unadmitted() {
    let mut invalid = Config::parse("[job.invalid]\nnamespace='team'\nruntime = \"process\"\nexec = \"/usr/bin/true\"\ncommand=[]\ncpu='100m'\n").unwrap();
    predictable_policy_refusal_keeps_batch_unadmitted(invalid.job.remove("invalid").unwrap()).await;
}

async fn predictable_policy_refusal_keeps_batch_unadmitted(
    invalid: reliaburger::config::job::JobSpec,
) {
    let scratch = tempfile::tempdir().unwrap();
    let records = scratch.path().join("records");
    let launches = scratch.path().join("launches");
    let runner = Harness::start_with(HarnessOptions {
        records_dir: Some(records.clone()),
        ..Default::default()
    })
    .await;
    let healthy = "batch-policy-healthy";
    let rejected = "batch-policy-invalid";
    let spec = serde_json::json!({
        "runtime":"process","exec":"/bin/sh", "namespace":"team",
        "command":["-c", "printf healthy >> \"$1\"", "sh", launches]
    });
    let request = serde_json::json!({
        "batch_id":991,
        "jobs":[
            {"name":healthy,"namespace":"team","spec":spec},
            {"name":rejected,"namespace":"team","spec":invalid}
        ],
        "execution_labels":{
            (healthy):{"name":"healthy","namespace":"team"},
            (rejected):{"name":"invalid","namespace":"team"}
        }
    });
    let http = reqwest::Client::new();
    let refused = http
        .post(format!("{}/v1/batch/run", runner.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&request)
        .send()
        .await
        .unwrap();
    let status = refused.status();
    let error = refused.text().await.unwrap();
    let checkpoint_exists = records.join("job-attempts.checkpoint").exists();
    let jobs: Vec<serde_json::Value> = http
        .get(format!("{}/v1/jobs", runner.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let instances: Vec<serde_json::Value> = http
        .get(format!("{}/v1/status", runner.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let launched_before_retry = launches.exists();
    let corrected = http
        .post(format!("{}/v1/batch/run", runner.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&serde_json::json!({
            "batch_id":991,
            "jobs":[{"name":healthy,"namespace":"team","spec":spec}],
            "execution_labels":{(healthy):{"name":"healthy","namespace":"team"}}
        }))
        .send()
        .await
        .unwrap();
    let corrected_status = corrected.status();
    let corrected_error = corrected.text().await.unwrap();
    if corrected_status == reqwest::StatusCode::ACCEPTED {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !launches.exists() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
    }
    drop(runner);
    assert!(!status.is_success(), "node policy was bypassed: {error}");
    assert!(
        !checkpoint_exists && jobs.is_empty() && instances.is_empty() && !launched_before_retry,
        "predictable refusal admitted ownership/runtime evidence: checkpoint={checkpoint_exists}, jobs={jobs:?}, instances={instances:?}, launched={launched_before_retry}; {error}"
    );
    assert_eq!(
        corrected_status,
        reqwest::StatusCode::ACCEPTED,
        "a healthy corrected request was permanently fenced: {corrected_error}"
    );
    assert_eq!(std::fs::read_to_string(launches).unwrap(), "healthy");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_log_rows_preserve_logical_scope_and_distinct_execution_ids() {
    let scratch = tempfile::tempdir().unwrap();
    let store = Arc::new(RwLock::new(reliaburger::ketchup::log_store::LogStore::new(
        scratch.path().to_path_buf(),
    )));
    let (sink, mut records) = mpsc::channel(16);
    let harness = Harness::start_with(HarnessOptions {
        auth_context: Some(reliaburger::sesame::auth::AuthContext {
            token_name: "reader".into(),
            principal_id: "reader-credential".into(),
            role: reliaburger::sesame::types::ApiRole::ReadOnly,
            scoped_apps: Some(vec!["migration".into()]),
            scoped_namespaces: Some(vec!["team".into()]),
        }),
        log_sink: Some(sink),
        log_store: Some(store.clone()),
        ..Default::default()
    })
    .await;
    let http = reqwest::Client::new();
    let executions = [
        "batch-11111111111111111111111111111111",
        "batch-22222222222222222222222222222222",
    ];
    for execution in executions {
        let response = http.post(format!("{}/v1/batch/run", harness.base_url))
            .bearer_auth(TEST_SERVICE_TOKEN)
            .json(&serde_json::json!({
                "batch_id": 99, "callback_base_url": null,
                "jobs": [{"name": execution, "namespace": "team", "spec": {
                    "runtime":"process","exec":"/bin/sh", "command": ["-c", "echo logical-sentinel; sleep 5"], "namespace": "team"
                }}],
                "execution_labels": {(execution): {"name": "migration", "namespace": "team"}}
            })).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    }
    for _ in executions {
        let record = tokio::time::timeout(Duration::from_secs(10), records.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            record.app, "migration",
            "forwarder stranded the logical query"
        );
        assert_eq!(record.namespace, "team");
        assert!(
            executions
                .iter()
                .any(|execution| record.instance.contains(execution))
        );
        store.write().await.ingest(&record);
    }
    let response = http
        .get(format!(
            "{}/v1/logs/entries/migration/team",
            harness.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let entries: Vec<reliaburger::ketchup::types::LogEntry> = response.json().await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_ne!(entries[0].instance, entries[1].instance);
    assert!(entries.iter().all(|entry| entry.line == "logical-sentinel"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_remote_ownership_cannot_relabel_a_local_ordinary_log_selector() {
    let council = single_node_leader().await;
    let ordinary = Harness::start_with(HarnessOptions {
        council: Some(council.clone()),
        node_name: Some("local-worker".into()),
        ..Default::default()
    })
    .await;
    let execution = "ordinary-execution";
    let id = reliaburger::grill::InstanceIdentity::new("team", execution, 0)
        .instance_id()
        .0;
    // Exercise the remaining legacy node-local selector with an actual legacy
    // supervisor instance; common runs use run-ID and separate scoped selection.
    let config = Config::parse(&format!("[job.{execution}]\nnamespace='team'\nruntime = \"process\"\nexec = \"/bin/sh\"\ncommand=['-c','echo ordinary-private-sentinel; sleep 10']")).unwrap();
    let (events, mut progress) = mpsc::channel(16);
    ordinary
        .cmd_tx
        .send(reliaburger::bun::agent::AgentCommand::Deploy { config, events })
        .await
        .unwrap();
    while progress.recv().await.is_some() {}

    let record: BatchRecord = serde_json::from_value(serde_json::json!({
        "jobs": [{"name": "migration", "execution_name": execution, "namespace": "team", "resources":{"cpu_millicores":0,"memory_bytes":0,"gpus":0}, "spec_digest": "a".repeat(64), "node": "remote-worker", "status": "Pending"}],
        "submitted_at_epoch_secs": epoch_now_secs()
    })).unwrap();
    write_admission_fixture(
        &council,
        reliaburger::council::types::RaftRequest::BatchRegister {
            expected_log_id: None,
            batch: record,
        },
    )
    .await
    .unwrap();
    let http = reqwest::Client::new();
    for prefix in ["logs", "logs/entries"] {
        let response = http
            .get(format!("{}/v1/{prefix}/migration/team", ordinary.base_url))
            .query(&[("instance", &id)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(
            !response
                .text()
                .await
                .unwrap()
                .contains("ordinary-private-sentinel")
        );
    }
    let logs = http
        .get(format!("{}/v1/logs/{execution}/team", ordinary.base_url))
        .query(&[("instance", &id)])
        .send()
        .await
        .unwrap();
    assert_eq!(
        logs.status(),
        reqwest::StatusCode::OK,
        "the ordinary owner must remain available under its own label"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_log_selector_uses_committed_ownership_and_original_scope() {
    use reliaburger::council::types::RaftRequest;
    use reliaburger::grill::InstanceIdentity;
    use reliaburger::ketchup::types::{LogRecord, LogStream};
    let council = single_node_leader().await;
    let scratch = tempfile::tempdir().unwrap();
    let store = Arc::new(RwLock::new(reliaburger::ketchup::log_store::LogStore::new(
        scratch.path().to_path_buf(),
    )));
    let old_execution = "batch-remote-old";
    let new_execution = "batch-remote-new";
    let old_id = InstanceIdentity::new("team", old_execution, 0)
        .instance_id()
        .0;
    let new_id = InstanceIdentity::new("team", new_execution, 0)
        .instance_id()
        .0;
    for (label, execution, id, line) in [
        (
            "migration",
            old_execution,
            old_id.as_str(),
            "old-execution-sentinel",
        ),
        (
            old_execution,
            new_execution,
            new_id.as_str(),
            "new-execution-sentinel",
        ),
    ] {
        let record: BatchRecord = serde_json::from_value(serde_json::json!({
            "jobs": [{"name": label, "execution_name": execution, "namespace": "team", "resources":{"cpu_millicores":0,"memory_bytes":0,"gpus":0}, "spec_digest": "a".repeat(64), "node": "remote-worker", "status": "Pending"}],
            "submitted_at_epoch_secs": epoch_now_secs()
        })).unwrap();
        write_admission_fixture(
            &council,
            RaftRequest::BatchRegister {
                expected_log_id: None,
                batch: record,
            },
        )
        .await
        .unwrap();
        store.write().await.ingest(&LogRecord {
            app: label.into(),
            namespace: "team".into(),
            instance: id.into(),
            stream: LogStream::Stdout,
            line: line.into(),
            position: None,
        });
    }
    let http = reqwest::Client::new();
    for reader_label in ["migration", old_execution] {
        // This council node holds no runtime, active job or retired node proof.
        let reader = Harness::start_with(HarnessOptions {
            council: Some(council.clone()),
            node_name: Some("leader".into()),
            log_store: Some(store.clone()),
            auth_context: Some(reliaburger::sesame::auth::AuthContext {
                token_name: "reader".into(),
                principal_id: "reader-credential".into(),
                role: reliaburger::sesame::types::ApiRole::ReadOnly,
                scoped_apps: Some(vec![reader_label.into()]),
                scoped_namespaces: Some(vec!["team".into()]),
            }),
            ..Default::default()
        })
        .await;
        let path = format!("{}/v1/logs/entries/{old_execution}/team", reader.base_url);
        let ambiguous = http.get(&path).send().await.unwrap();
        assert_eq!(ambiguous.status(), reqwest::StatusCode::BAD_REQUEST);
        for (label, id, sentinel) in [
            ("migration", old_id.as_str(), "old-execution-sentinel"),
            (old_execution, new_id.as_str(), "new-execution-sentinel"),
        ] {
            let response = http
                .get(&path)
                .query(&[("instance", id)])
                .send()
                .await
                .unwrap();
            let status = response.status();
            let body = response.text().await.unwrap();
            if label == reader_label {
                assert_eq!(status, reqwest::StatusCode::OK, "{reader_label}: {body}");
                let entries: Vec<reliaburger::ketchup::types::LogEntry> =
                    serde_json::from_str(&body).unwrap();
                assert_eq!(entries.len(), 1, "{body}");
                assert_eq!(entries[0].instance.as_deref(), Some(id));
                assert_eq!(entries[0].line, sentinel);
            } else {
                assert_eq!(
                    status,
                    reqwest::StatusCode::FORBIDDEN,
                    "{reader_label}: {body}"
                );
                assert!(!body.contains(sentinel));
            }
        }
        for id in ["team__batch-never-owned-0", "other__batch-remote-old-0"] {
            let unknown = http
                .get(&path)
                .query(&[("instance", id)])
                .send()
                .await
                .unwrap();
            assert_eq!(unknown.status(), reqwest::StatusCode::BAD_REQUEST);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambiguous_batch_log_paths_require_an_instance_with_its_own_logical_scope() {
    let old_execution = "batch-11111111111111111111111111111111";
    let new_execution = "batch-22222222222222222222222222222222";
    for reader_name in ["migration", old_execution] {
        let scratch = tempfile::tempdir().unwrap();
        let store = Arc::new(RwLock::new(reliaburger::ketchup::log_store::LogStore::new(
            scratch.path().to_path_buf(),
        )));
        let (sink, mut records) = mpsc::channel(16);
        let runner = Harness::start_with(HarnessOptions {
            auth_context: Some(reliaburger::sesame::auth::AuthContext {
                token_name: "reader".into(),
                principal_id: "reader-credential".into(),
                role: reliaburger::sesame::types::ApiRole::ReadOnly,
                scoped_apps: Some(vec![reader_name.into()]),
                scoped_namespaces: Some(vec!["team".into()]),
            }),
            log_sink: Some(sink),
            log_store: Some(store.clone()),
            ..Default::default()
        })
        .await;
        let http = reqwest::Client::new();
        for (execution, label, sentinel) in [
            (old_execution, "migration", "old-sentinel"),
            (new_execution, old_execution, "new-sentinel"),
        ] {
            let response = http.post(format!("{}/v1/batch/run", runner.base_url))
                .bearer_auth(TEST_SERVICE_TOKEN)
                .json(&serde_json::json!({
                    "batch_id": 99,
                    "jobs": [{"name": execution, "namespace": "team", "spec": {
                        "image": "proc-grill:image-ignored", "command": ["sh", "-c", format!("echo {sentinel}; sleep 5")], "namespace": "team"
                    }}],
                    "execution_labels": {(execution): {"name": label, "namespace": "team"}}
                })).send().await.unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
        }
        for _ in 0..2 {
            let record = tokio::time::timeout(Duration::from_secs(10), records.recv())
                .await
                .unwrap()
                .unwrap();
            store.write().await.ingest(&record);
        }
        for route in ["logs", "logs/entries"] {
            let ambiguous = http
                .get(format!(
                    "{}/v1/{route}/{old_execution}/team",
                    runner.base_url
                ))
                .send()
                .await
                .unwrap();
            assert_eq!(ambiguous.status(), reqwest::StatusCode::BAD_REQUEST);
            for (execution, label, sentinel, other) in [
                (old_execution, "migration", "old-sentinel", "new-sentinel"),
                (new_execution, old_execution, "new-sentinel", "old-sentinel"),
            ] {
                let id =
                    reliaburger::grill::InstanceIdentity::new("team", execution, 0).instance_id();
                let response = http
                    .get(format!(
                        "{}/v1/{route}/{old_execution}/team",
                        runner.base_url
                    ))
                    .query(&[("instance", id.0.as_str())])
                    .send()
                    .await
                    .unwrap();
                if reader_name == label {
                    assert_eq!(response.status(), reqwest::StatusCode::OK);
                    let body = response.text().await.unwrap();
                    assert!(body.contains(sentinel), "{body}");
                    assert!(!body.contains(other), "selector mixed executions: {body}");
                } else {
                    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_runner_status_and_logs_use_the_logical_scope_for_distinct_executions() {
    let harness = Harness::start_with(HarnessOptions {
        auth_context: Some(reliaburger::sesame::auth::AuthContext {
            token_name: "reader".into(),
            principal_id: "reader-credential".into(),
            role: reliaburger::sesame::types::ApiRole::ReadOnly,
            scoped_apps: Some(vec!["migration".into()]),
            scoped_namespaces: Some(vec!["team".into()]),
        }),
        ..Default::default()
    })
    .await;
    let http = reqwest::Client::new();
    let executions = [
        "batch-11111111111111111111111111111111",
        "batch-22222222222222222222222222222222",
    ];
    for execution in executions {
        let response = http.post(format!("{}/v1/batch/run", harness.base_url))
            .bearer_auth(TEST_SERVICE_TOKEN)
            .json(&serde_json::json!({
                "batch_id": 99, "callback_base_url": null,
                "jobs": [{"name": execution, "namespace": "team", "spec": {
                    "runtime":"process","exec":"/bin/sh", "command": ["-c", "echo logical-sentinel; sleep 5"], "namespace": "team"
                }}],
                "execution_labels": {(execution): {"name": "migration", "namespace": "team"}}
            })).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let rows: Vec<serde_json::Value> = http
                .get(format!("{}/v1/jobs", harness.base_url))
                .bearer_auth(TEST_SERVICE_TOKEN)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if rows.len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let rows: Vec<serde_json::Value> = http
        .get(format!("{}/v1/jobs", harness.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "logical reader lost both admitted executions: {rows:?}"
    );
    assert!(
        rows.iter()
            .all(|row| row["name"] == "migration" && row["namespace"] == "team")
    );
    for execution in executions {
        let response = http
            .get(format!("{}/v1/logs/{execution}/team", harness.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn internal_batch_dispatch_refuses_missing_extra_invalid_or_cross_namespace_labels() {
    let harness = Harness::start().await;
    let http = reqwest::Client::new();
    for labels in [
        serde_json::Value::Null,
        serde_json::json!({}),
        serde_json::json!({"batch-111": {"name": "migration", "namespace": "team"}, "extra": {"name": "extra", "namespace": "team"}}),
        serde_json::json!({"batch-111": {"name": "../migration", "namespace": "team"}}),
        serde_json::json!({"batch-111": {"name": "migration", "namespace": "other"}}),
    ] {
        let mut body = serde_json::json!({
            "batch_id": 99, "callback_base_url": null,
            "jobs": [{"name": "batch-111", "namespace": "team", "spec": {"runtime":"process","exec":"/usr/bin/true", "command": [], "namespace": "team"}}],
        });
        if !labels.is_null() {
            body["execution_labels"] = labels;
        }
        let response = http
            .post(format!("{}/v1/batch/run", harness.base_url))
            .bearer_auth(TEST_SERVICE_TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(matches!(
            response.status(),
            reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::UNPROCESSABLE_ENTITY
        ));
    }
    let rows: Vec<serde_json::Value> = http
        .get(format!("{}/v1/jobs", harness.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(rows.is_empty());
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
                AgentCommand::RunJobsWithLabels {
                    events, response, ..
                } => {
                    // Acknowledge owned admission while holding launch evidence.
                    held_events.push(events);
                    let _ = response.send(Ok(BTreeMap::new()));
                }
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
    let first = harness.client.submit_batch(&jobs).await.unwrap();
    let second = harness.client.submit_batch(&jobs).await.unwrap();
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
    let response = harness.client.submit_batch(&jobs).await.unwrap();
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
    let response = harness.client.submit_batch(&jobs).await.unwrap();
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
    let response = harness.client.submit_batch(&jobs).await.unwrap();
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
        runtime = "process"
        exec = "/bin/echo"
        namespace = "batchspace"
        command = ["hi"]
    "#,
    );
    let response = harness.client.submit_batch(&jobs).await.unwrap();
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
        "execution_labels": {"remote-1": {"name": "remote-1", "namespace": "default"}},
        "jobs": [{
            "name": "remote-1",
            "spec": { "runtime":"process","exec":"/bin/echo", "command": ["remote"] },
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
        .json(&serde_json::json!({ "job_name": "remote-1", "namespace": "default", "status": "completed", "exit_code": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(report.status().as_u16(), 404);
}

/// JOB3: `/v1/batch/{id}/report` validates its input — forged states
/// are 400, illegal transitions are 409, duplicates are an idempotent
/// 200 that never double-counts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_callbacks_cannot_forge_or_change_common_run_outcomes() {
    let harness = Harness::start().await;

    let jobs = jobs_from(
        r#"
        [job.steady]
        runtime = "process"
        exec = "/bin/echo"
        command = ["hi"]
    "#,
    );
    let response = harness.client.submit_batch(&jobs).await.unwrap();
    let batch_id = response["batch_id"].as_u64().unwrap();
    let execution = response["executions"]["steady"].as_str().unwrap();
    harness.wait_done(batch_id, 30).await;

    let http = reqwest::Client::new();
    let report_url = format!("{}/v1/batch/{batch_id}/report", harness.base_url);

    // A made-up status string is a 400.
    let forged = http
        .post(&report_url)
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&serde_json::json!({ "job_name": execution, "namespace": "default", "status": "meltdown", "exit_code": null }))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status().as_u16(), 400);

    // Legacy callback claims cannot alter the common accepted grant.
    let duplicate = http
        .post(&report_url)
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&serde_json::json!({ "job_name": execution, "namespace": "default", "status": "completed", "exit_code": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status().as_u16(), 404);
    let summary = harness.client.batch_status(batch_id).await.unwrap();
    assert_eq!(summary["succeeded"].as_u64(), Some(1), "no double count");

    // A conflicting callback cannot replace the accepted result either.
    let conflict = http
        .post(&report_url)
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&serde_json::json!({ "job_name": execution, "namespace": "default", "status": "failed", "exit_code": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status().as_u16(), 404);

    // A job that was never part of the batch is a 404.
    let unknown = http
        .post(&report_url)
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&serde_json::json!({ "job_name": "impostor", "namespace": "default", "status": "completed", "exit_code": 0 }))
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
        "execution_labels": {"x": {"name": "x", "namespace": "default"}},
        "jobs": [{
            "name": "x",
            "spec": { "runtime":"process","exec":"/bin/echo", "command": ["x"] },
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
        .json(&serde_json::json!({ "job_name": "x", "namespace": "default", "status": "completed", "exit_code": 0 }))
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
        runtime = "process"
        exec = "/bin/sleep"
        command = ["300"]
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
    let response = first.client.submit_batch(&jobs).await.unwrap();
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
            api_advertised: true,
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
            api_advertised: true,
        },
        NodeMembershipInfo {
            node_id: reliaburger::meat::NodeId("runner".to_string()),
            address: runner_address,
            api_advertised: true,
        },
    ];

    let leader = Harness::start_with(HarnessOptions {
        council: Some(Arc::clone(&council)),
        node_name: Some("leader".to_string()),
        membership: Some(membership),
        capacity_nodes: vec!["runner".to_string()],
        ..Default::default()
    })
    .await;

    // Submit: the only capacity is the runner, so the job dispatches
    // there with a callback URL pointing at the dead port.
    let jobs = jobs_from(
        r#"
        [job.faraway]
        runtime = "process"
        exec = "/bin/echo"
        command = ["far"]
    "#,
    );
    let response = leader.client.submit_batch(&jobs).await.unwrap();
    let batch_id = response["batch_id"].as_u64().unwrap();
    assert_eq!(response["assigned"].as_u64(), Some(1), "{response}");

    // The pull watcher completes the batch despite the lost callbacks.
    let summary = leader.wait_done(batch_id, 60).await;
    assert_eq!(summary["succeeded"].as_u64(), Some(1), "{summary}");

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
    assert_eq!(summary["succeeded"].as_u64(), Some(1), "{summary}");
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
            resources: reliaburger::meat::Resources::default(),
            name: "orphan".to_string(),
            execution_name: "orphan".to_string(),
            spec_digest: "a".repeat(64),
            namespace: "default".to_string(),
            node: Some(reliaburger::meat::NodeId("runner".to_string())),
            status: JobStatus::Pending,
        }],
        submitted_at_epoch_secs: epoch_now_secs(),
    };
    let response = write_admission_fixture(
        &council,
        reliaburger::council::types::RaftRequest::BatchRegister {
            expected_log_id: None,
            batch: record,
        },
    )
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
        "execution_labels": {"orphan": {"name": "orphan", "namespace": "default"}},
        "jobs": [{
            "name": "orphan",
            "spec": { "runtime":"process","exec":"/bin/echo", "command": ["orphan"] },
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
        api_advertised: true,
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

// Batch allocations bypass app placements. Follow routes must select the
// committed execution owner and retain the original logical scope boundary.

async fn batch_follow_peer(
    sentinel: &'static str,
    selected: String,
) -> (
    std::net::SocketAddr,
    Arc<std::sync::atomic::AtomicUsize>,
    TestTasks,
) {
    use axum::extract::{Path, Query};
    use axum::http::HeaderMap;
    use axum::response::sse::{Event, Sse};
    use futures_util::StreamExt as _;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    let peer = axum::Router::new().route(
        "/v1/logs/{app}/{namespace}",
        axum::routing::get(
            move |Path((app, namespace)): Path<(String, String)>,
                  Query(query): Query<BTreeMap<String, String>>,
                  headers: HeaderMap| {
                let calls = observed.clone();
                let selected = selected.clone();
                async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    assert_eq!((app.as_str(), namespace.as_str()), ("migration", "team"));
                    assert_eq!(query.get("follow").map(String::as_str), Some("true"));
                    assert_eq!(query.get("local").map(String::as_str), Some("true"));
                    assert_eq!(query.get("label").map(String::as_str), Some("true"));
                    assert_eq!(query.get("instance"), Some(&selected));
                    assert_eq!(
                        headers
                            .get("authorization")
                            .and_then(|value| value.to_str().ok()),
                        Some(format!("Bearer {TEST_SERVICE_TOKEN}").as_str()),
                    );
                    let events = futures_util::stream::once(async move {
                        Ok::<_, std::convert::Infallible>(Event::default().data(sentinel))
                    })
                    .chain(futures_util::stream::pending());
                    Sse::new(events)
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let stopped = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, peer)
            .with_graceful_shutdown(async move { stopped.cancelled().await })
            .await
            .unwrap();
    });
    (address, calls, TestTasks::new(shutdown, vec![server]))
}

fn batch_follow_reader_scope(app: &str) -> reliaburger::sesame::auth::AuthContext {
    reliaburger::sesame::auth::AuthContext {
        token_name: "reader".into(),
        principal_id: "reader-credential".into(),
        role: reliaburger::sesame::types::ApiRole::ReadOnly,
        scoped_apps: Some(vec![app.into()]),
        scoped_namespaces: Some(vec!["team".into()]),
    }
}

async fn assert_batch_follow_selects_committed_owner(
    websocket: bool,
    same_label_app: bool,
    unavailable: Option<&str>,
) {
    use futures_util::StreamExt as _;
    use reliaburger::council::types::RaftRequest;
    use reliaburger::meat::types::{AppId, NodeId, Placement, Resources, SchedulingDecision};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let local = matches!(unavailable, Some("local-empty" | "local-capture"));
    let local_capture = unavailable == Some("local-capture");
    let local_spec = Config::parse("[job.migration]\nruntime = \"process\"\nexec = \"/bin/echo\"\nnamespace='team'\ncommand=['worker-local-capture-sentinel']\n")
        .unwrap().job.remove("migration").unwrap();
    use sha2::{Digest, Sha256};
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&("team", "migration", &local_spec)).unwrap())
    );
    let council = single_node_leader().await;
    let execution = "batch-remote-follow";
    let selected = reliaburger::grill::InstanceIdentity::new("team", execution, 0)
        .instance_id()
        .0;
    let record: BatchRecord = serde_json::from_value(serde_json::json!({
        "jobs": [{"name": "migration", "execution_name": execution, "namespace": "team", "resources":{"cpu_millicores":0,"memory_bytes":0,"gpus":0}, "spec_digest": if local_capture { digest } else { "a".repeat(64) }, "node": if local_capture { "reader-node" } else { "batch-worker" }, "status": "Pending"}],
        "submitted_at_epoch_secs": if unavailable == Some("pruned") { 0 } else { epoch_now_secs() }
    })).unwrap();
    write_admission_fixture(
        &council,
        RaftRequest::BatchRegister {
            expected_log_id: None,
            batch: record,
        },
    )
    .await
    .unwrap();
    if unavailable == Some("pruned") {
        council
            .write(RaftRequest::BatchJobUpdate {
                batch_id: 1,
                job_name: execution.into(),
                namespace: "team".into(),
                status: JobStatus::Completed,
                exit_code: Some(0),
            })
            .await
            .unwrap();
        write_admission_fixture(
            &council,
            RaftRequest::BatchRegister {
                expected_log_id: None,
                batch: BatchRecord {
                    jobs: Vec::new(),
                    submitted_at_epoch_secs: epoch_now_secs(),
                },
            },
        )
        .await
        .unwrap();
        assert!(
            !council
                .desired_state()
                .await
                .batch_state
                .batches
                .iter()
                .any(|(id, _)| *id == 1)
        );
    }
    if same_label_app {
        let mut ordinary =
            Config::parse("[app.migration]\nimage='proc-grill:image-ignored'\nnamespace='team'\n")
                .unwrap();
        council
            .write(RaftRequest::AppSpec {
                app_id: AppId::new("migration", "team"),
                spec: Box::new(ordinary.app.remove("migration").unwrap()),
            })
            .await
            .unwrap();
        write_admission_fixture(
            &council,
            RaftRequest::SchedulingDecision(SchedulingDecision {
                app_id: AppId::new("migration", "team"),
                placements: vec![Placement {
                    node_id: NodeId("ordinary-worker".into()),
                    resources: Resources::new(0, 0, 0),
                    ordinal: 0,
                }],
            }),
        )
        .await
        .unwrap();
    }
    let desired = council.desired_state().await;
    assert!(
        !desired
            .scheduling
            .contains_key(&AppId::new(execution, "team"))
    );
    assert_eq!(
        desired
            .scheduling
            .contains_key(&AppId::new("migration", "team")),
        same_label_app,
    );
    assert_eq!(
        desired
            .batch_state
            .execution_owner("team", execution)
            .unwrap()
            .logical_name,
        "migration",
    );
    let (batch_address, batch_calls, _batch_server) =
        batch_follow_peer("committed-batch-owner-sentinel", selected.clone()).await;
    let (ordinary_address, ordinary_calls, _ordinary_server) =
        batch_follow_peer("unrelated-app-placement-sentinel", selected.clone()).await;
    let mut membership = vec![
        NodeMembershipInfo {
            node_id: NodeId("batch-worker".into()),
            address: batch_address,
            api_advertised: true,
        },
        NodeMembershipInfo {
            node_id: NodeId("ordinary-worker".into()),
            address: ordinary_address,
            api_advertised: true,
        },
    ];
    if unavailable == Some("missing") {
        membership.retain(|member| member.node_id.0 != "batch-worker");
    } else if unavailable == Some("unadvertised") {
        membership[0].api_advertised = false;
    }
    let reader = Harness::start_with(HarnessOptions {
        council: Some(council.clone()),
        node_name: Some("reader-node".into()),
        membership: Some(membership.clone()),
        auth_context: Some(batch_follow_reader_scope("migration")),
        ..Default::default()
    })
    .await;
    let denied = Harness::start_with(HarnessOptions {
        council: Some(council.clone()),
        node_name: Some("denied-reader-node".into()),
        membership: Some(membership),
        auth_context: Some(batch_follow_reader_scope("not-migration")),
        ..Default::default()
    })
    .await;
    let http = reqwest::Client::new();
    if local_capture {
        let run = http
            .post(format!("{}/v1/batch/run", reader.base_url))
            .bearer_auth(TEST_SERVICE_TOKEN)
            .json(&serde_json::json!({
                "batch_id": 1,
                "jobs": [{"name": execution, "namespace": "team", "spec": local_spec}],
                "execution_labels": {(execution): {"name": "migration", "namespace": "team"}}
            }))
            .send()
            .await
            .unwrap();
        let status = run.status();
        let error = run.text().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::ACCEPTED, "{error}");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let logs = http
                    .get(format!("{}/v1/logs/{execution}/team", reader.base_url))
                    .query(&[("instance", selected.as_str())])
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap();
                if logs.contains("worker-local-capture-sentinel") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("local batch capture did not become ready");
    }
    if websocket {
        let denied_url = format!(
            "{}/v1/ws/logs/{execution}/team?instance={selected}&local={local}&tail=10",
            denied.base_url.replace("http://", "ws://"),
        );
        match tokio_tungstenite::connect_async(denied_url).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                assert_eq!(response.status().as_u16(), 403);
            }
            other => panic!("an unrelated logical scope acquired a batch follow: {other:?}"),
        }
        let url = format!(
            "{}/v1/ws/logs/{execution}/team?instance={selected}&local={local}&tail=10",
            reader.base_url.replace("http://", "ws://"),
        );
        if unavailable.is_some() && !local {
            match tokio_tungstenite::connect_async(url).await {
                Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                    assert_eq!(response.status().as_u16(), 503)
                }
                other => panic!("unavailable batch allocation acquired a WebSocket: {other:?}"),
            }
            assert_eq!(batch_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert_eq!(ordinary_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            drop(denied);
            drop(reader);
            council.shutdown().await.unwrap();
            return;
        }
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("selected worker never supplied or closed its WebSocket");
        if unavailable == Some("local-empty") {
            assert!(
                !matches!(next, Some(Ok(WsMessage::Text(_)))),
                "local-only follow acquired a remote line: {next:?}"
            );
            assert_eq!(batch_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert_eq!(ordinary_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            drop(denied);
            drop(reader);
            council.shutdown().await.unwrap();
            return;
        }
        let frame = next.unwrap().unwrap();
        let WsMessage::Text(text) = frame else {
            panic!("expected a batch-owner text frame");
        };
        assert_eq!(
            serde_json::from_str::<reliaburger::ketchup::follow::LogFrame>(&text).unwrap(),
            reliaburger::ketchup::follow::LogFrame::Line(
                if local_capture {
                    "worker-local-capture-sentinel"
                } else {
                    "committed-batch-owner-sentinel"
                }
                .into()
            ),
            "a same-label app placement was treated as a batch execution owner",
        );
        socket.close(None).await.unwrap();
    } else {
        let denied_response = http
            .get(format!("{}/v1/logs/{execution}/team", denied.base_url))
            .query(&[
                ("follow", "true"),
                ("instance", selected.as_str()),
                ("local", if local { "true" } else { "false" }),
                ("tail", "10"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(denied_response.status(), reqwest::StatusCode::FORBIDDEN);
        let response = http
            .get(format!("{}/v1/logs/{execution}/team", reader.base_url))
            .query(&[
                ("follow", "true"),
                ("instance", selected.as_str()),
                ("local", if local { "true" } else { "false" }),
                ("tail", "10"),
            ])
            .send()
            .await
            .unwrap();
        if unavailable.is_some() && !local {
            assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(batch_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert_eq!(ordinary_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            drop(denied);
            drop(reader);
            council.shutdown().await.unwrap();
            return;
        }
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let mut body = response.bytes_stream();
        let next = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .expect("selected worker never supplied or closed its SSE stream");
        if unavailable == Some("local-empty") {
            assert!(
                next.is_none(),
                "local-only follow acquired remote SSE data: {next:?}"
            );
            assert_eq!(batch_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert_eq!(ordinary_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            drop(denied);
            drop(reader);
            council.shutdown().await.unwrap();
            return;
        }
        let chunk = next.unwrap().unwrap();
        let text = std::str::from_utf8(&chunk).unwrap();
        assert!(
            text.contains(if local_capture {
                "worker-local-capture-sentinel"
            } else {
                "committed-batch-owner-sentinel"
            }),
            "{text}"
        );
        assert!(!text.contains("unrelated-app-placement-sentinel"), "{text}");
    }
    assert_eq!(
        batch_calls.load(std::sync::atomic::Ordering::SeqCst),
        if local { 0 } else { 1 }
    );
    assert_eq!(
        ordinary_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "unrelated same-label app placement was contacted for the batch selector",
    );
    drop(denied);
    drop(reader);
    council.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_sse_follow_uses_committed_remote_owner_without_app_placement() {
    assert_batch_follow_selects_committed_owner(false, false, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_sse_follow_does_not_substitute_same_label_app_placement() {
    assert_batch_follow_selects_committed_owner(false, true, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_websocket_follow_uses_committed_remote_owner_without_app_placement() {
    assert_batch_follow_selects_committed_owner(true, false, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_websocket_follow_does_not_substitute_same_label_app_placement() {
    assert_batch_follow_selects_committed_owner(true, true, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_sse_follow_refuses_missing_allocation() {
    assert_batch_follow_selects_committed_owner(false, true, Some("missing")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_sse_follow_refuses_unadvertised_allocation() {
    assert_batch_follow_selects_committed_owner(false, true, Some("unadvertised")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_sse_follow_refuses_pruned_allocation() {
    assert_batch_follow_selects_committed_owner(false, true, Some("pruned")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_websocket_follow_refuses_missing_allocation() {
    assert_batch_follow_selects_committed_owner(true, true, Some("missing")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_websocket_follow_refuses_unadvertised_allocation() {
    assert_batch_follow_selects_committed_owner(true, true, Some("unadvertised")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_websocket_follow_refuses_pruned_allocation() {
    assert_batch_follow_selects_committed_owner(true, true, Some("pruned")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_sse_follow_local_empty_never_uses_app_placements() {
    assert_batch_follow_selects_committed_owner(false, true, Some("local-empty")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_sse_follow_local_capture_never_uses_app_placements() {
    assert_batch_follow_selects_committed_owner(false, true, Some("local-capture")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_websocket_follow_local_empty_never_uses_app_placements() {
    assert_batch_follow_selects_committed_owner(true, true, Some("local-empty")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_websocket_follow_local_capture_never_uses_app_placements() {
    assert_batch_follow_selects_committed_owner(true, true, Some("local-capture")).await;
}

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
    let retry =
        tokio::time::timeout(Duration::from_secs(3), new_leader.client.apply(&config)).await;
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
        assert!(
            retry.is_err(),
            "an idempotent repeat must observe the same pending operation"
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_positively_failed_migration_releases_its_claim_for_a_corrected_apply_on_the_same_node() {
    let council = single_node_leader().await;
    let app_id = reliaburger::meat::AppId::new("api", "default");
    council
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
    let scratch = tempfile::tempdir().unwrap();
    let records = scratch.path().join("instances");
    let attempts = scratch.path().join("attempts");
    let first = Harness::start_with(HarnessOptions {
        council: Some(council.clone()),
        node_name: Some("same-node".into()),
        records_dir: Some(records.clone()),
        ..Default::default()
    })
    .await;
    let mut config = Config::parse(
        "[app.api]\nimage='new'\n[job.migrate]\nruntime = \"process\"\nexec = \"/bin/sh\"\nrun_before=['app.api']\n",
    )
    .unwrap();
    config.job.get_mut("migrate").unwrap().command = Some(vec![
        "-c".into(),
        format!("printf 'failed\\n' >> '{}'; exit 1", attempts.display()),
    ]);
    let failed = tokio::time::timeout(Duration::from_secs(10), first.client.apply(&config))
        .await
        .expect("failed migration never returned its settled result");
    assert!(failed.is_err(), "{failed:?}");
    let state = council.desired_state().await;
    assert_eq!(state.apps[&app_id].image.as_deref(), Some("old"));
    assert!(
        state.prerequisite_claims.is_empty(),
        "a positively observed and durably settled failure must release its claim"
    );
    let failed_run = state
        .task_arrays
        .jobs()
        .runs()
        .find(|(_, run)| run.name == "migrate")
        .unwrap()
        .0;
    let failed_record = state.task_arrays.get(failed_run).unwrap();
    assert_eq!(failed_record.state.spec.max_attempts, 1);
    assert_eq!(failed_record.state.summary().failed, 1);
    assert_eq!(std::fs::read_to_string(&attempts).unwrap(), "failed\n");
    tokio::time::timeout(Duration::from_secs(5), async {
        while council
            .desired_state()
            .await
            .task_arrays
            .deployment_owner("migrate", "default")
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    drop(first);
    let corrected = Harness::start_with(HarnessOptions {
        council: Some(council.clone()),
        node_name: Some("same-node".into()),
        records_dir: Some(records.clone()),
        ..Default::default()
    })
    .await;
    assert_eq!(
        council
            .desired_state()
            .await
            .task_arrays
            .get(failed_run)
            .unwrap()
            .state
            .summary()
            .failed,
        1
    );
    config.job.get_mut("migrate").unwrap().command = Some(vec![
        "-c".into(),
        format!("printf 'corrected\\n' >> '{}'; exit 0", attempts.display()),
    ]);
    let result = tokio::time::timeout(Duration::from_secs(10), corrected.client.apply(&config))
        .await
        .expect("corrected migration never settled");
    assert!(
        result.is_ok(),
        "corrected apply remained blocked: {result:?}"
    );
    let state = council.desired_state().await;
    assert_eq!(state.apps[&app_id].image.as_deref(), Some("new"));
    assert!(state.prerequisite_claims.is_empty());
    let completed_run = state
        .task_arrays
        .jobs()
        .runs()
        .filter(|(_, run)| run.name == "migrate")
        .map(|(id, _)| id)
        .max()
        .unwrap();
    assert!(completed_run > failed_run);
    assert_eq!(
        state
            .task_arrays
            .get(completed_run)
            .unwrap()
            .state
            .summary()
            .succeeded,
        1
    );
    assert_eq!(
        state
            .task_arrays
            .get(completed_run)
            .unwrap()
            .state
            .spec
            .max_attempts,
        1
    );
    assert_eq!(
        std::fs::read_to_string(&attempts).unwrap(),
        "failed\ncorrected\n",
        "an automatic retry must not repeat the failed side effect"
    );
    drop(corrected);
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

async fn write_admission_fixture(
    council: &reliaburger::council::CouncilNode,
    mut request: reliaburger::council::RaftRequest,
) -> Result<reliaburger::council::CouncilResponse, reliaburger::council::CouncilError> {
    let previous = council.desired_state().await.last_applied_log;
    match &mut request {
        reliaburger::council::RaftRequest::BatchRegister {
            expected_log_id, ..
        } => *expected_log_id = previous,
        reliaburger::council::RaftRequest::SchedulingDecision(decision) => {
            request = reliaburger::council::RaftRequest::SchedulingDecisions {
                expected_log_id: previous,
                decisions: vec![decision.clone()],
            }
        }
        _ => {}
    }
    council.write(request).await
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
    let response = harness.client.submit_batch(&jobs).await.unwrap();
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
            ))
            .await
            .unwrap();
        assert_eq!(response["assigned"], 1, "{mode}: {response}");
        let id = response["batch_id"].as_u64().unwrap();
        let summary = harness.wait_done(id, 10).await;
        assert_eq!(summary["succeeded"], 1, "{mode}: {summary}");
        let desired = council.desired_state().await;
        assert!(desired.batch_state.batches.is_empty());
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
