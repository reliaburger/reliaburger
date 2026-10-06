//! Task arrays through the real API, Raft and node executor (million-jobs
//! plan, M5).
//!
//! One node with its API on an ephemeral port: `POST /v1/batch/array`
//! registers the array (through a single-node council, or in memory when
//! standalone), the leader loop syncs the node's own executor, and
//! `GET /v1/batch/{id}` shows the array's summary. The executor runs real
//! processes where the test is about processes, and the fake runner where
//! it's about numbers.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reliaburger::bun::api;
use reliaburger::bun::task_array_leader::TaskArrayService;
use reliaburger::bun::task_array_node::{NodeRunner, TaskArrayNode, TaskArrayNodeConfig};
use reliaburger::bun::task_executor::{AttemptOutcome, FakeRunner, ProcessRunner};
use reliaburger::bun::task_ledger::GroupCommit;
use reliaburger::config::process_workloads::ProcessWorkloadsConfig;
use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::task_harness::TestTasks;

const SHELL: &str = "/bin/sh";
const NODE: &str = "node-1";

struct Harness {
    base_url: String,
    http: reqwest::Client,
    council: Option<Arc<CouncilNode>>,
    _data: tempfile::TempDir,
    _tasks: TestTasks,
}

struct Options {
    council: bool,
    runner: NodeRunner,
    allowed: Vec<&'static str>,
    slots: u32,
}

impl Options {
    fn processes(council: bool) -> Self {
        Self {
            council,
            runner: NodeRunner::Process(ProcessRunner::default()),
            allowed: vec![SHELL],
            slots: 8,
        }
    }
}

async fn single_node_leader() -> Arc<CouncilNode> {
    let router = InMemoryRaftRouter::new();
    let network = InMemoryRaftNetworkFactory::new(1, router.clone());
    let config = CouncilConfig {
        heartbeat_interval_ms: 50,
        election_timeout_min_ms: 150,
        election_timeout_max_ms: 400,
        snapshot_threshold: 1000,
        max_in_snapshot_log_to_keep: 500,
    };
    let node = CouncilNode::new(
        1,
        config,
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
        CouncilNodeInfo::new("127.0.0.1:9001".parse().unwrap(), NODE.to_string()),
    );
    node.initialize(members).await.unwrap();
    let node = Arc::new(node);
    for _ in 0..80 {
        if node.is_leader().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    node
}

impl Harness {
    async fn start(options: Options) -> Self {
        Self::start_with_tokens(options, None).await
    }

    async fn start_with_tokens(
        options: Options,
        tokens: Option<reliaburger::sesame::auth::TokenStore>,
    ) -> Self {
        Self::start_with_worker_files(options, tokens, |_| {}).await
    }

    async fn start_with_worker_files(
        options: Options,
        tokens: Option<reliaburger::sesame::auth::TokenStore>,
        prepare: impl FnOnce(&std::path::Path),
    ) -> Self {
        let shutdown = CancellationToken::new();
        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        // No agent: task arrays don't go through it. Holding the receiver
        // keeps the API's background loops alive until shutdown.
        let agent_shutdown = shutdown.clone();
        let agent = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = agent_shutdown.cancelled() => return,
                    command = cmd_rx.recv() => if command.is_none() { return },
                }
            }
        });

        let data = tempfile::tempdir().unwrap();
        prepare(data.path());
        let node = TaskArrayNode::new(
            TaskArrayNodeConfig {
                root: data.path().join("task-arrays"),
                policy: ProcessWorkloadsConfig {
                    allowed_binaries: options.allowed.iter().map(PathBuf::from).collect(),
                    mount_isolation: false,
                    ..ProcessWorkloadsConfig::default()
                },
                default_concurrency: options.slots,
                backoff: (Duration::from_millis(1), Duration::from_millis(5)),
                group_commit: GroupCommit {
                    interval: Duration::from_millis(10),
                    max_records: 4096,
                },
            },
            options.runner,
        );
        let service = Arc::new(TaskArrayService::with_timings(
            Some(Arc::new(node)),
            Duration::from_millis(50),
            Duration::from_secs(30),
        ));
        let council = if options.council {
            Some(single_node_leader().await)
        } else {
            None
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = api::router_with_upgrade(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            council.clone(),
            tokens,
            None,
            None,
            None,
            None,
            None,
            port,
            None,
            None,
            None,
            "default".to_string(),
            Some(NODE.to_string()),
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
            None,
            Some(service),
        );
        let server_shutdown = shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { server_shutdown.cancelled().await })
                .await
                .ok();
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            http: reqwest::Client::new(),
            council,
            _data: data,
            _tasks: TestTasks::new(shutdown, vec![agent, server]),
        }
    }

    async fn submit(&self, body: Value) -> (u16, Value) {
        let response = self
            .http
            .post(format!("{}/v1/batch/array", self.base_url))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn submit_ok(&self, body: Value) -> u64 {
        let (status, answer) = self.submit(body).await;
        assert_eq!(status, 202, "{answer}");
        answer["batch_id"].as_u64().unwrap()
    }

    async fn status(&self, batch_id: u64) -> Value {
        self.http
            .get(format!("{}/v1/batch/{batch_id}", self.base_url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn wait_until(&self, batch_id: u64, secs: u64, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let summary = self.status(batch_id).await;
            if done(&summary) {
                return summary;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "array {batch_id} not there in {secs}s: {summary}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_done(&self, batch_id: u64, secs: u64) -> Value {
        self.wait_until(batch_id, secs, |s| s["done"] == true).await
    }

    async fn get(&self, path: &str) -> (u16, Vec<u8>) {
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .send()
            .await
            .unwrap();
        (
            response.status().as_u16(),
            response.bytes().await.unwrap().to_vec(),
        )
    }
}

fn shell_array(count: u32, chunk_size: u32, script: &str) -> Value {
    json!({
        "name": "render",
        "template": { "exec": SHELL, "command": ["-c", script] },
        "spec": { "count": count, "chunk_size": chunk_size },
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_leader_snapshot_still_reconciles_old_worker_files() {
    let harness = Harness::start_with_worker_files(Options::processes(false), None, |root| {
        let orphan = root.join("task-arrays/999");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("ledger"), b"old worker evidence").unwrap();
    })
    .await;
    let orphan = harness._data.path().join("task-arrays/999");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while orphan.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "an empty snapshot never reached the worker"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_array_of_processes_runs_to_completion_standalone() {
    let harness = Harness::start(Options::processes(false)).await;
    let batch_id = harness
        .submit_ok(shell_array(300, 50, "test -n \"$RELIABURGER_TASK_INDEX\""))
        .await;
    let summary = harness.wait_done(batch_id, 60).await;
    assert_eq!(summary["kind"], "array");
    assert_eq!(summary["status"], "Succeeded", "{summary}");
    assert_eq!(summary["succeeded"], 300);
    assert_eq!(summary["chunks_done"], 6);
    assert_eq!(summary["nodes"][0]["node"], NODE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failures_are_counted_and_their_output_kept() {
    let harness = Harness::start(Options::processes(true)).await;
    let mut array = shell_array(40, 10, "echo out {index}; test {index} -ne 7");
    array["spec"]["max_attempts"] = json!(2);
    let batch_id = harness.submit_ok(array).await;
    let summary = harness.wait_done(batch_id, 60).await;
    assert_eq!(summary["status"], "CompletedWithFailures", "{summary}");
    assert_eq!(summary["succeeded"], 39);
    assert_eq!(summary["failed"], 1);
    assert_eq!(summary["retried"], 1);
    assert_eq!(summary["failed_indices"], json!([[7, 7]]));

    let (status, body) = harness
        .get(&format!("/v1/batch/{batch_id}/results?failed=true"))
        .await;
    assert_eq!(status, 200);
    let results: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        results["rows"],
        json!([{ "grant_attempt": 1, "index": 7, "attempts": 2, "succeeded": false, "not_run": false, "exit_code": 1, "run_ms": results["rows"][0]["run_ms"] }])
    );
    let (_, body) = harness
        .get(&format!("/v1/batch/{batch_id}/results?limit=5"))
        .await;
    let results: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(results["rows"].as_array().unwrap().len(), 5);
    assert_eq!(results["truncated"], true);

    let (status, body) = harness
        .get(&format!("/v1/batch/{batch_id}/tasks/7/logs"))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body, b"out 7\n");
    let (status, _) = harness
        .get(&format!("/v1/batch/{batch_id}/tasks/3/logs"))
        .await;
    assert_eq!(status, 404, "succeeded tasks keep no output");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hundred_thousand_tasks_cost_a_few_hundred_raft_entries() {
    let harness = Harness::start(Options {
        council: true,
        runner: NodeRunner::Fake(FakeRunner::new(Duration::ZERO, |task| {
            AttemptOutcome::Exited {
                code: i32::from(task.index % 1000 == 7 && task.attempt == 1),
            }
        })),
        allowed: vec![SHELL],
        slots: 64,
    })
    .await;
    let council = harness.council.clone().unwrap();
    let before = council
        .raft()
        .metrics()
        .borrow()
        .last_log_index
        .unwrap_or(0);

    let batch_id = harness
        .submit_ok(json!({
            "name": "many",
            "template": { "exec": SHELL, "command": ["{index}"] },
            "spec": { "count": 100_000 },
        }))
        .await;
    let summary = harness.wait_done(batch_id, 120).await;
    assert_eq!(summary["status"], "Succeeded", "{summary}");
    assert_eq!(summary["succeeded"], 100_000);
    assert_eq!(summary["retried"], 100);
    assert_eq!(summary["chunks_done"], 98);

    // Registration plus at most one entry per sync that changed
    // something: never anywhere near one per task.
    let entries = council
        .raft()
        .metrics()
        .borrow()
        .last_log_index
        .unwrap_or(0)
        - before;
    assert!(entries <= 300, "{entries} Raft entries for 100,000 tasks");
    eprintln!("task_arrays: 100,000 tasks through Raft in {entries} entries");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_an_array_stops_it() {
    let harness = Harness::start(Options {
        council: true,
        runner: NodeRunner::Fake(FakeRunner::new(Duration::from_secs(60), |_| {
            AttemptOutcome::Exited { code: 0 }
        })),
        allowed: vec![SHELL],
        slots: 4,
    })
    .await;
    let batch_id = harness.submit_ok(shell_array(1000, 100, "exit 0")).await;
    harness
        .wait_until(batch_id, 30, |s| s["nodes"][0]["counters"]["running"] == 4)
        .await;

    let response = harness
        .http
        .post(format!("{}/v1/batch/{batch_id}/cancel", harness.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 202);
    let summary = harness.wait_done(batch_id, 30).await;
    assert_eq!(summary["status"], "Cancelled", "{summary}");
    assert_eq!(summary["not_run"], 1000);
    assert_eq!(summary["succeeded"], 0);

    let response = harness
        .http
        .post(format!("{}/v1/batch/999/cancel", harness.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_submission_is_refused_with_the_reason() {
    let harness = Harness::start(Options::processes(true)).await;
    let (status, answer) = harness
        .submit(json!({
            "name": "images",
            "template": { "image": "alpine:3", "exec": SHELL, "command": ["true"] },
            "spec": { "count": 10 },
        }))
        .await;
    assert_eq!(status, 400);
    assert!(
        answer["error"].as_str().unwrap().contains("exec"),
        "{answer}"
    );

    let (status, answer) = harness.submit(shell_array(0, 10, "exit 0")).await;
    assert_eq!(status, 400);
    assert!(
        answer["error"].as_str().unwrap().contains("count"),
        "{answer}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_may_not_run_the_binary_says_why() {
    let harness = Harness::start(Options {
        allowed: vec!["/usr/bin/true"],
        ..Options::processes(true)
    })
    .await;
    let batch_id = harness.submit_ok(shell_array(20, 10, "exit 0")).await;
    let summary = harness
        .wait_until(batch_id, 30, |s| s["nodes"][0]["refused"].is_string())
        .await;
    assert!(
        summary["nodes"][0]["refused"]
            .as_str()
            .unwrap()
            .contains("allowed_binaries")
    );
    assert_eq!(summary["status"], "Running");
    assert_eq!(
        summary["queued"], 20,
        "nothing is granted to a node that can't run it"
    );
}

/// One Raft submission accepts all profiles; the summary list never expands indexes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_manifest_has_atomic_identity_histograms_and_indexed_pages() {
    let harness = Harness::start(Options {
        council: true,
        runner: NodeRunner::Fake(FakeRunner::new(Duration::ZERO, |task| {
            AttemptOutcome::Exited {
                code: i32::from(task.index == 5000),
            }
        })),
        allowed: vec![SHELL],
        slots: 16,
    })
    .await;
    let cohort = |name: &str, count: u32, cpu: &str, memory: &str| json!({"name":name,"count":count,"chunk_size":256,"max_attempts":1,"template":{"image":"fixture:v1","command":["worker","{index}"],"cpu":cpu,"memory":memory}});
    let response = harness.http.post(format!("{}/v1/batch/manifest",harness.base_url)).json(&json!({"name":"mixed","namespace":"tenant-a","cohort":[cohort("small",6000,"100m","32Mi"),cohort("large",16,"1000m","64Mi")]})).send().await.unwrap();
    assert_eq!(response.status(), 202);
    let id = response.json::<Value>().await.unwrap()["batch_id"]
        .as_u64()
        .unwrap();
    let summary = harness.wait_done(id, 60).await;
    assert_eq!(summary["total"], 6016);
    assert_eq!(summary["succeeded"], 6015);
    assert_eq!(summary["failed"], 1);
    assert_eq!(summary["cohorts"].as_array().unwrap().len(), 2);
    let counts: u64 = summary["duration_final_attempt_ms"]["counts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .sum();
    assert_eq!(counts, 6016);
    let child = summary["cohorts"][0]["batch_id"].as_u64().unwrap();
    let (status, page) = harness
        .get(&format!("/v1/batch/{child}/results?failed=true&limit=20"))
        .await;
    assert_eq!(status, 200);
    let page: Value = serde_json::from_slice(&page).unwrap();
    assert!(page["rows"].as_array().unwrap().is_empty());
    assert_eq!(page["next_after"], 4095);
    let (_, page) = harness
        .get(&format!(
            "/v1/batch/{child}/results?failed=true&limit=20&after=4095"
        ))
        .await;
    let page: Value = serde_json::from_slice(&page).unwrap();
    assert_eq!(page["rows"][0]["index"], 5000);
    let (_, page) = harness
        .get(&format!("/v1/batch/{child}/results?index=5000"))
        .await;
    let page: Value = serde_json::from_slice(&page).unwrap();
    assert_eq!(page["rows"].as_array().unwrap().len(), 1);
    let (_, listing) = harness.get("/v1/batch/summaries").await;
    let listing: Value = serde_json::from_slice(&listing).unwrap();
    assert_eq!(listing["batches"].as_array().unwrap().len(), 1);
    assert_eq!(listing["batches"][0]["batch_id"], id);
    let (_, html) = harness.get("/ui/fragment/batches").await;
    assert!(String::from_utf8(html).unwrap().contains("6015 / 6016"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_profile_refuses_the_whole_manifest_and_parent_cancel_drains_all_profiles() {
    let harness = Harness::start(Options {
        council: true,
        runner: NodeRunner::Fake(FakeRunner::new(Duration::from_millis(100), |_| {
            AttemptOutcome::Exited { code: 0 }
        })),
        allowed: vec![SHELL],
        slots: 2,
    })
    .await;
    let profile = |name: &str, count: u32| json!({"name":name,"count":count,"chunk_size":10,"template":{"exec":SHELL,"command":["-c","exit 0"]}});
    let invalid = json!({"name":"mixed","cohort":[profile("small",100),profile("large",0)]});
    let response = harness
        .http
        .post(format!("{}/v1/batch/manifest", harness.base_url))
        .json(&invalid)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(
        harness
            .council
            .as_ref()
            .unwrap()
            .desired_state()
            .await
            .task_arrays
            .ids()
            .is_empty()
    );
    let valid = json!({"name":"mixed","cohort":[profile("small",100),profile("large",100)]});
    let response = harness
        .http
        .post(format!("{}/v1/batch/manifest", harness.base_url))
        .json(&valid)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let id = response.json::<Value>().await.unwrap()["batch_id"]
        .as_u64()
        .unwrap();
    assert_eq!(id, 1, "invalid submission consumed IDs");
    let response = harness
        .http
        .post(format!("{}/v1/batch/{id}/cancel", harness.base_url))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let summary = harness.wait_done(id, 30).await;
    assert_eq!(summary["status"], "Cancelled");
    assert_eq!(
        summary["total"].as_u64().unwrap(),
        summary["succeeded"].as_u64().unwrap()
            + summary["failed"].as_u64().unwrap()
            + summary["not_run"].as_u64().unwrap()
    );
    for child in summary["cohorts"].as_array().unwrap() {
        assert_eq!(child["status"], "Cancelled");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manifest_admission_and_summary_views_honour_the_callers_scope() {
    use reliaburger::sesame::types::{ApiRole, TokenScope};
    let operator = reliaburger::sesame::token::create_token(
        "operator",
        ApiRole::Admin,
        TokenScope::default(),
        None,
    )
    .unwrap();
    let scoped = reliaburger::sesame::token::create_token(
        "scoped",
        ApiRole::Deployer,
        TokenScope {
            apps: Some(vec!["allowed".into()]),
            namespaces: Some(vec!["tenant-a".into()]),
        },
        None,
    )
    .unwrap();
    let tokens = reliaburger::sesame::auth::new_token_store();
    tokens.write().await.extend([operator.token, scoped.token]);
    let harness = Harness::start_with_tokens(
        Options {
            council: true,
            runner: NodeRunner::Fake(FakeRunner::new(Duration::ZERO, |_| {
                AttemptOutcome::Exited { code: 0 }
            })),
            allowed: vec![SHELL],
            slots: 2,
        },
        Some(tokens),
    )
    .await;
    let request = |name: &str, namespace: &str| json!({"name":name,"namespace":namespace,"cohort":[{"name":"small","count":1,"template":{"exec":SHELL,"command":["-c","true"]}}]});
    let submit = |name: &str, namespace: &str, token: &str| {
        harness
            .http
            .post(format!("{}/v1/batch/manifest", harness.base_url))
            .bearer_auth(token)
            .json(&request(name, namespace))
    };
    for (name, namespace) in [("other", "tenant-a"), ("allowed", "tenant-b")] {
        assert_eq!(
            submit(name, namespace, &scoped.plaintext)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
    let allowed = submit("allowed", "tenant-a", &scoped.plaintext)
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), 202);
    let allowed_id = allowed.json::<Value>().await.unwrap()["batch_id"]
        .as_u64()
        .unwrap();
    let hidden = submit("other", "tenant-b", &operator.plaintext)
        .send()
        .await
        .unwrap();
    assert_eq!(hidden.status(), 202);
    let hidden_id = hidden.json::<Value>().await.unwrap()["batch_id"]
        .as_u64()
        .unwrap();
    let visible: Value = harness
        .http
        .get(format!("{}/v1/batch/summaries", harness.base_url))
        .bearer_auth(&scoped.plaintext)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(visible["batches"].as_array().unwrap().len(), 1);
    assert_eq!(visible["batches"][0]["batch_id"], allowed_id);
    let html = harness
        .http
        .get(format!("{}/ui/fragment/batches", harness.base_url))
        .bearer_auth(&scoped.plaintext)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("allowed"));
    assert!(!html.contains("other"));
    assert_eq!(
        harness
            .http
            .post(format!("{}/v1/batch/{hidden_id}/cancel", harness.base_url))
            .bearer_auth(&scoped.plaintext)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
}
