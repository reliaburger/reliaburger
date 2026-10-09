//! Task arrays on a real in-process cluster (million-jobs plan, M5).
//!
//! Three fully wired nodes (gossip, Raft, reporting, the HTTP API and
//! the leader loop, as `bun --cluster` runs them), each with its own
//! task-array executor on a fake runner and a real ledger. The array is
//! submitted to a follower, which forwards it to the leader; the leader
//! syncs all three nodes over HTTP. Partway through, one follower is
//! killed while it holds chunks: the leader must take them back after the
//! silence timeout and the array must still finish with every task
//! counted exactly once.
//!
//! Gated behind `RELIABURGER_CLUSTER_TESTS=1` like the other multi-node
//! suites. Run via `make test-cluster`, or
//! `RELIABURGER_CLUSTER_TESTS=1 cargo test --test cluster_task_arrays -- --ignored`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reliaburger::bun::task_array_leader::TaskArrayService;
use reliaburger::bun::task_array_node::{NodeRunner, TaskArrayNode, TaskArrayNodeConfig};
use reliaburger::bun::task_executor::{AttemptOutcome, FakeRunner};
use reliaburger::bun::task_ledger::GroupCommit;
use reliaburger::config::process_workloads::ProcessWorkloadsConfig;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[path = "support/cluster.rs"]
mod cluster_support;
use cluster_support::{
    MembershipSource, WiredNode, WiredNodeOptions, cluster_tests_enabled, local, start_wired_node,
};

const SERVICE_TOKEN: &str = "cluster-test-task-arrays-service";
const BASE_PORT: u16 = 23510;
const BINARY: &str = "/usr/local/bin/rb-task";
const TASKS: u32 = 100_000;

fn service(data: &std::path::Path) -> Arc<TaskArrayService> {
    let node = TaskArrayNode::new(
        TaskArrayNodeConfig {
            root: data.join("task-arrays"),
            policy: ProcessWorkloadsConfig {
                allowed_binaries: vec![PathBuf::from(BINARY)],
                mount_isolation: false,
                ..ProcessWorkloadsConfig::default()
            },
            default_concurrency: 16,
            backoff: (Duration::from_millis(1), Duration::from_millis(5)),
            group_commit: GroupCommit {
                interval: Duration::from_millis(20),
                max_records: 4096,
            },
        },
        // A small delay per task keeps chunks in flight long enough for
        // the killed node to be holding some.
        NodeRunner::Fake(FakeRunner::new(Duration::from_micros(200), |task| {
            AttemptOutcome::Exited {
                code: i32::from(task.index % 500 == 3 && task.attempt == 1),
            }
        })),
    );
    Arc::new(TaskArrayService::with_timings(
        Some(Arc::new(node)),
        Duration::from_millis(200),
        Duration::from_secs(3),
    ))
}

async fn start_node(
    index: usize,
    seeds: Vec<std::net::SocketAddr>,
    root: &CancellationToken,
    data: &std::path::Path,
) -> WiredNode {
    start_wired_node(WiredNodeOptions {
        name: format!("ta{index}"),
        gossip_port: BASE_PORT + (index as u16) * 10,
        seeds,
        shutdown: root.child_token(),
        data_dir_prefix: "rb-task-arrays",
        stale_report_timeout_secs: 10,
        metrics_rollup: None,
        scheduler: None,
        lease_reaper: false,
        membership: MembershipSource::Gossip,
        service_identity: Some(SERVICE_TOKEN.into()),
        operator_token: None,
        fault_injection: false,
        labels: Default::default(),
        keep_data_dir: false,
        task_arrays: Some(service(data)),
    })
    .await
}

/// The voter set every node has applied, once they all agree on a
/// uniform (not joint) configuration.
fn settled_voters(nodes: &[WiredNode]) -> Option<BTreeSet<u64>> {
    let mut agreed: Option<(openraft::LogId<u64>, BTreeSet<u64>)> = None;
    for node in nodes {
        let metrics = node.metrics_rx.borrow();
        let stored = &metrics.membership_config;
        let membership = stored.membership();
        let log_id = (*stored.log_id())?;
        if membership.get_joint_config().len() != 1
            || metrics.last_applied.is_none_or(|applied| applied < log_id)
        {
            return None;
        }
        let voters: BTreeSet<u64> = membership.voter_ids().collect();
        match &agreed {
            None => agreed = Some((log_id, voters)),
            Some((id, set)) if *id == log_id && *set == voters => {}
            Some(_) => return None,
        }
    }
    agreed.map(|(_, voters)| voters)
}

async fn wait_until(what: &str, timeout: Duration, mut cond: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond().await {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn status(http: &reqwest::Client, node: &WiredNode, batch_id: u64) -> Option<Value> {
    http.get(format!(
        "http://127.0.0.1:{}/v1/batch/{batch_id}",
        node.api_port
    ))
    .send()
    .await
    .ok()?
    .json()
    .await
    .ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn a_task_array_survives_losing_a_node_mid_run() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );
    let root = CancellationToken::new();
    let data: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut nodes = vec![start_node(0, vec![], &root, data[0].path()).await];
    for (index, directory) in data.iter().enumerate().skip(1) {
        nodes.push(start_node(index, vec![local(BASE_PORT)], &root, directory.path()).await);
    }
    // Killing a node before the voter set settles could leave the others
    // on a joint configuration that still needs it.
    wait_until(
        "three settled voters",
        Duration::from_secs(60),
        async || settled_voters(&nodes).is_some_and(|voters| voters.len() == 3),
    )
    .await;
    wait_until(
        "gossip sees everyone",
        Duration::from_secs(60),
        async || {
            for node in &nodes {
                if node.membership_table.read().await.len() < 3 {
                    return false;
                }
            }
            true
        },
    )
    .await;
    let leader = {
        let mut found = None;
        for (index, node) in nodes.iter().enumerate() {
            if node.council.is_leader().await {
                found = Some(index);
            }
        }
        found.expect("a leader")
    };
    let follower = (leader + 1) % 3;
    let victim = (leader + 2) % 3;

    // Submit through a follower: it forwards to the leader.
    let http = reqwest::Client::new();
    let answer: Value = http
        .post(format!(
            "http://127.0.0.1:{}/v1/batch/array",
            nodes[follower].api_port
        ))
        .json(&json!({
            "name": "survivor",
            "template": { "exec": BINARY, "command": ["{index}"] },
            "spec": { "count": TASKS },
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let batch_id = answer["batch_id"]
        .as_u64()
        .unwrap_or_else(|| panic!("{answer}"));
    let raft_before = nodes[leader]
        .metrics_rx
        .borrow()
        .last_log_index
        .unwrap_or(0);

    // Wait until the victim is running work, then kill it.
    let victim_name = format!("ta{victim}");
    wait_until(
        "the victim holds work",
        Duration::from_secs(60),
        async || {
            status(&http, &nodes[leader], batch_id)
                .await
                .is_some_and(|summary| {
                    summary["nodes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|node| {
                            node["node"] == victim_name.as_str()
                                && node["counters"]["succeeded"].as_u64().unwrap_or(0) > 0
                        })
                })
        },
    )
    .await;
    nodes[victim].shutdown.cancel();

    let mut last = Value::Null;
    wait_until(
        "the array finishes",
        Duration::from_secs(180),
        async || match status(&http, &nodes[leader], batch_id).await {
            Some(summary) => {
                let done = summary["done"] == true;
                last = summary;
                done
            }
            None => false,
        },
    )
    .await;
    assert_eq!(last["status"], "Succeeded", "{last}");
    assert_eq!(last["succeeded"], TASKS, "{last}");
    assert_eq!(last["failed"], 0);
    assert_eq!(
        last["retried"],
        TASKS / 500,
        "exactly one retry per flaky index"
    );

    let entries = nodes[leader]
        .metrics_rx
        .borrow()
        .last_log_index
        .unwrap_or(0)
        - raft_before;
    assert!(entries < 2_000, "{entries} Raft entries for {TASKS} tasks");
    eprintln!("cluster_task_arrays: {TASKS} tasks, one node lost, {entries} Raft entries");

    // Every surviving node's results are reachable, and between them and
    // the lost node's retired chunks the leader counted every task.
    let results: Value = http
        .get(format!(
            "http://127.0.0.1:{}/v1/batch/{batch_id}/results?failed=true",
            nodes[follower].api_port
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(results["rows"], json!([]), "{results}");
    // Mixed resource profiles follow the same follower-to-leader admission path.
    let mixed = http.post(format!("http://127.0.0.1:{}/v1/batch/manifest", nodes[follower].api_port))
        .json(&json!({"name":"mixed-survivor","namespace":"default","cohort":[
            {"name":"small","count":20,"template":{"exec":BINARY,"command":["{index}"],"cpu":"100m","memory":"32Mi"}},
            {"name":"large","count":12,"template":{"exec":BINARY,"command":["{index}"],"cpu":"1000m","memory":"64Mi"}}
        ]})).send().await.unwrap();
    assert_eq!(mixed.status(), 202);
    let mixed_id = mixed.json::<Value>().await.unwrap()["batch_id"]
        .as_u64()
        .unwrap();
    wait_until(
        "forwarded manifest finishes",
        Duration::from_secs(30),
        async || {
            status(&http, &nodes[follower], mixed_id)
                .await
                .is_some_and(|summary| {
                    if summary["done"] != true {
                        return false;
                    }
                    assert_eq!(summary["succeeded"], 32);
                    assert_eq!(summary["cohorts"].as_array().unwrap().len(), 2);
                    true
                })
        },
    )
    .await;
    root.cancel();
}

async fn start_common_node(
    index: usize,
    seeds: Vec<std::net::SocketAddr>,
    root: &CancellationToken,
    data: &std::path::Path,
    delay: Duration,
    operator: &reliaburger::sesame::types::ApiToken,
) -> WiredNode {
    let executor = TaskArrayNode::new(
        TaskArrayNodeConfig {
            root: data.join("task-arrays"),
            policy: ProcessWorkloadsConfig {
                allowed_binaries: vec![BINARY.into()],
                mount_isolation: false,
                ..Default::default()
            },
            default_concurrency: 2,
            backoff: (Duration::from_millis(1), Duration::from_millis(5)),
            group_commit: GroupCommit::default(),
        },
        NodeRunner::Fake(FakeRunner::new(delay, |_| AttemptOutcome::Exited {
            code: 0,
        })),
    );
    start_wired_node(WiredNodeOptions {
        name: format!("common{index}"),
        gossip_port: BASE_PORT + 100 + index as u16 * 10,
        seeds,
        shutdown: root.child_token(),
        data_dir_prefix: "rb-common-jobs",
        stale_report_timeout_secs: 10,
        metrics_rollup: None,
        scheduler: None,
        lease_reaper: false,
        membership: MembershipSource::Gossip,
        service_identity: Some(SERVICE_TOKEN.into()),
        operator_token: Some(operator.clone()),
        fault_injection: false,
        labels: Default::default(),
        keep_data_dir: false,
        task_arrays: Some(Arc::new(TaskArrayService::with_timings(
            Some(Arc::new(executor)),
            Duration::from_millis(100),
            Duration::from_secs(3),
        ))),
    })
    .await
}

async fn common_cluster(
    delay: Duration,
) -> (
    CancellationToken,
    Vec<tempfile::TempDir>,
    Vec<WiredNode>,
    String,
) {
    let operator = reliaburger::sesame::token::create_token(
        "common-operator",
        reliaburger::sesame::types::ApiRole::Admin,
        Default::default(),
        None,
    )
    .unwrap();
    let root = CancellationToken::new();
    let data: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut nodes = Vec::new();
    for (index, directory) in data.iter().enumerate() {
        nodes.push(
            start_common_node(
                index,
                if index == 0 {
                    vec![]
                } else {
                    vec![local(BASE_PORT + 100)]
                },
                &root,
                directory.path(),
                delay,
                &operator.token,
            )
            .await,
        );
    }
    wait_until(
        "common cluster's three settled voters",
        Duration::from_secs(60),
        async || settled_voters(&nodes).is_some_and(|voters| voters.len() == 3),
    )
    .await;
    wait_until(
        "common cluster gossip roster",
        Duration::from_secs(60),
        async || {
            for node in &nodes {
                if node.membership_table.read().await.len() < 3 {
                    return false;
                }
            }
            true
        },
    )
    .await;
    (root, data, nodes, operator.plaintext)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a real multi-node cluster; run with make test-cluster"]
async fn a_durable_cron_occurrence_keeps_its_run_identity_across_leadership_change() {
    assert!(cluster_tests_enabled());
    let (root, _data, nodes, token) = common_cluster(Duration::from_secs(2)).await;
    let leader = nodes
        .iter()
        .position(|node| *node.thinks_leader.borrow())
        .unwrap();
    let follower = (leader + 1) % 3;
    let http = reqwest::Client::new();
    // Registering a schedule marks the minute it lands in as already
    // observed. A request sent in the last moments of a minute can land in
    // the target minute and skip it, so start well inside one.
    while time::OffsetDateTime::now_utc().second() >= 45 {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // One upcoming minute, so a later legitimate occurrence cannot look like a duplicate.
    let next = time::OffsetDateTime::now_utc() + time::Duration::minutes(1);
    let expression = format!(
        "{} {} {} {} *",
        next.minute(),
        next.hour(),
        next.day(),
        u8::from(next.month())
    );
    let response = http.post(format!("http://127.0.0.1:{}/v1/jobs/runs", nodes[follower].api_port)).bearer_auth(&token)
        .json(&json!({"name":"scheduled-singleton","definition":{"template":{"exec":BINARY},"cron":{"expression":expression}}})).send().await.unwrap();
    assert_eq!(response.status(), 202);
    let mut id = None;
    wait_until(
        "actual UTC cron tick",
        Duration::from_secs(75),
        async || {
            id = nodes[leader]
                .council
                .desired_state()
                .await
                .task_arrays
                .jobs()
                .runs()
                .find(|(_, run)| run.name == "scheduled-singleton")
                .map(|(id, _)| id);
            id.is_some()
        },
    )
    .await;
    let id = id.unwrap();
    eprintln!("common cluster: UTC cron admitted run {id}; moving leadership");
    // Trigger a real election while keeping all workers and quorum alive.
    // Removing a voter is not a handover: the self-healing council may restore it.
    // One election doesn't always move leadership: voters refuse a candidate
    // while the leader's lease holds, or while their log is ahead of the
    // candidate's, and the old leader can win the election that follows its
    // step-down. So ask again every two seconds until another node leads.
    let mut last_election: Option<tokio::time::Instant> = None;
    wait_until(
        "replacement cron leader",
        Duration::from_secs(30),
        async || {
            if nodes[follower].council.is_leader().await
                || nodes[(leader + 2) % 3].council.is_leader().await
            {
                return true;
            }
            if last_election.is_none_or(|at| at.elapsed() >= Duration::from_secs(2)) {
                nodes[follower]
                    .council
                    .raft()
                    .trigger()
                    .elect()
                    .await
                    .unwrap();
                last_election = Some(tokio::time::Instant::now());
            }
            false
        },
    )
    .await;
    let new_leader = if nodes[follower].council.is_leader().await {
        follower
    } else {
        (leader + 2) % 3
    };
    wait_until(
        "accepted scheduled singleton",
        Duration::from_secs(15),
        async || {
            nodes[new_leader]
                .council
                .desired_state()
                .await
                .task_arrays
                .get(id)
                .is_some_and(|record| record.state.summary().succeeded == 1)
        },
    )
    .await;
    let arrays = nodes[new_leader].council.desired_state().await.task_arrays;
    assert_eq!(
        arrays
            .jobs()
            .runs()
            .filter(|(_, run)| run.name == "scheduled-singleton")
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        vec![id]
    );
    let revision = arrays
        .jobs()
        .definition("default", "scheduled-singleton")
        .unwrap()
        .revision;
    let minute = next.unix_timestamp() / 60;
    let replayed = nodes[new_leader]
        .council
        .write(reliaburger::council::RaftRequest::TaskArray(Box::new(
            reliaburger::meat::task_array_store::TaskArrayWrite::Job(Box::new(
                reliaburger::meat::job::JobWrite::Fire {
                    name: "scheduled-singleton".into(),
                    namespace: "default".into(),
                    revision,
                    minute,
                    now_epoch_secs: (minute * 60) as u64,
                },
            )),
        )))
        .await
        .unwrap();
    // A processed cron tick is acknowledged as a no-op, including a skipped
    // occurrence. The retained run, rather than the acknowledgement, owns its ID.
    assert!(
        matches!(
            replayed,
            reliaburger::council::CouncilResponse::Applied { .. }
        ),
        "{replayed:?}"
    );
    let after = nodes[new_leader].council.desired_state().await.task_arrays;
    assert_eq!(
        after
            .jobs()
            .runs()
            .filter(|(_, run)| run.name == "scheduled-singleton")
            .map(|(run, _)| run)
            .collect::<Vec<_>>(),
        vec![id],
        "a replay after a real handover must not admit a second occurrence"
    );
    assert_eq!(after.get(id).unwrap().state.summary().succeeded, 1);
    root.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a real multi-node cluster; run with make test-cluster"]
async fn losing_a_singleton_worker_requires_exact_operator_replay_before_another_attempt() {
    assert!(cluster_tests_enabled());
    let (root, _data, nodes, token) = common_cluster(Duration::from_secs(10)).await;
    let leader = nodes
        .iter()
        .position(|node| *node.thinks_leader.borrow())
        .unwrap();
    let follower = (leader + 1) % 3;
    let http = reqwest::Client::new();
    let response = http.post(format!("http://127.0.0.1:{}/v1/jobs/runs", nodes[follower].api_port)).bearer_auth(&token)
        .json(&json!({"name":"side-effects","request_id":"first","definition":{"template":{"exec":BINARY},"tasks":{"max_attempts":1},"replay_unknown":false}})).send().await.unwrap();
    assert_eq!(response.status(), 202);
    let id = response.json::<Value>().await.unwrap()["batch_id"]
        .as_u64()
        .unwrap();
    let mut owner = None;
    wait_until(
        "actual worker attempt acknowledgement",
        Duration::from_secs(8),
        async || {
            let answer = http
                .get(format!(
                    "http://127.0.0.1:{}/v1/batch/{id}",
                    nodes[leader].api_port
                ))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap();
            owner = answer["nodes"]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["counters"]["running"] == 1))
                .and_then(|row| row["node"].as_str())
                .map(str::to_owned);
            owner.is_some()
        },
    )
    .await;
    let owner = owner.unwrap();
    let victim = nodes.iter().position(|node| node.name == owner).unwrap();
    nodes[victim].shutdown.cancel();
    let survivors: Vec<_> = (0..3).filter(|index| *index != victim).collect();
    wait_until(
        "surviving singleton leader",
        Duration::from_secs(30),
        async || {
            nodes[survivors[0]].council.is_leader().await
                || nodes[survivors[1]].council.is_leader().await
        },
    )
    .await;
    let current = if nodes[survivors[0]].council.is_leader().await {
        survivors[0]
    } else {
        survivors[1]
    };
    let read_follower = survivors
        .iter()
        .copied()
        .find(|index| *index != current)
        .unwrap();
    let mut proof = Value::Null;
    wait_until(
        "durable unknown owner",
        Duration::from_secs(20),
        async || {
            proof = http
                .get(format!(
                    "http://127.0.0.1:{}/v1/batch/{id}",
                    nodes[read_follower].api_port
                ))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap();
            proof["status"] == "Unknown"
        },
    )
    .await;
    assert_eq!(proof["succeeded"], 0);
    assert_eq!(proof["failed"], 0);
    assert_eq!(proof["held"], 1);
    assert_eq!(proof["queued"], 0);
    assert_eq!(proof["unknown_owners"][0]["node"], owner);
    let replay = format!(
        "http://127.0.0.1:{}/v1/jobs/runs/{id}/replay",
        nodes[read_follower].api_port
    );
    let stale = http
        .post(&replay)
        .bearer_auth(&token)
        .json(&json!({"node":owner,"grant_digest":"0".repeat(64),"acknowledged":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    let accepted = http.post(&replay).bearer_auth(&token).json(&json!({"node":owner,"grant_digest":proof["unknown_owners"][0]["grant_digest"],"acknowledged":true})).send().await.unwrap();
    assert_eq!(accepted.status(), 202);
    wait_until(
        "accepted replay outcome",
        Duration::from_secs(30),
        async || {
            nodes[current]
                .council
                .desired_state()
                .await
                .task_arrays
                .get(id)
                .is_some_and(|record| record.state.summary().succeeded == 1)
        },
    )
    .await;
    let record = nodes[current]
        .council
        .desired_state()
        .await
        .task_arrays
        .get(id)
        .unwrap()
        .clone();
    assert_eq!(record.state.summary().failed, 0);
    assert_ne!(
        record
            .state
            .accepted_grant(reliaburger::meat::task_array::ChunkId(0))
            .unwrap()
            .0
            .0,
        owner
    );
    root.cancel();
}
