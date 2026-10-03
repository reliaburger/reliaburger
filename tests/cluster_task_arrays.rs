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
    root.cancel();
}
