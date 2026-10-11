//! Task arrays on a real in-process cluster (million-jobs plan, M5).
//!
//! Three fully wired nodes (gossip, Raft, reporting, the HTTP API and
//! the leader loop, as `bun --cluster` runs them), each with its own
//! task-array executor on a fake runner and a real ledger. The array is
//! submitted to a follower, which forwards it to the leader; the leader
//! syncs all three nodes over HTTP. Partway through, one follower is
//! killed while it holds chunks: the leader must take them back after the
//! silence timeout and the array must still finish with every task
//! counted exactly once. A second test kills the leader itself while it
//! holds chunks: the new leader's silence clock must take them back and
//! the array must still count every task exactly once.
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

#[path = "support/bun_process.rs"]
mod bun_process;
#[path = "support/cluster.rs"]
mod cluster_support;
use bun_process::{PortBlockReservation, reserve_port_block_lease};
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
            audit_dir: None,
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
    base: u16,
    index: usize,
    seeds: Vec<std::net::SocketAddr>,
    root: &CancellationToken,
    data: &std::path::Path,
) -> WiredNode {
    start_wired_node(WiredNodeOptions {
        name: format!("ta{index}"),
        gossip_port: base + (index as u16) * 10,
        seeds,
        shutdown: root.child_token(),
        data_dir_prefix: "rb-task-arrays",
        data_root: None,
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
        task_arrays_on_agent_budget: None,
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
    let mut nodes = vec![start_node(BASE_PORT, 0, vec![], &root, data[0].path()).await];
    for (index, directory) in data.iter().enumerate().skip(1) {
        nodes.push(
            start_node(
                BASE_PORT,
                index,
                vec![local(BASE_PORT)],
                &root,
                directory.path(),
            )
            .await,
        );
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
            "template": { "runtime":"process","exec": BINARY, "command": ["{index}"] },
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
            {"name":"small","count":20,"template":{"runtime":"process","exec":BINARY,"command":["{index}"],"cpu":"100m","memory":"32Mi"}},
            {"name":"large","count":12,"template":{"runtime":"process","exec":BINARY,"command":["{index}"],"cpu":"1000m","memory":"64Mi"}}
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

/// Ports for the leader-loss test, clear of the other tests' blocks.
const LEADER_LOSS_BASE_PORT: u16 = BASE_PORT + 200;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn a_task_array_finishes_exactly_once_after_the_leader_is_killed_mid_run() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );
    let root = CancellationToken::new();
    let data: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut nodes = vec![start_node(LEADER_LOSS_BASE_PORT, 0, vec![], &root, data[0].path()).await];
    for (index, directory) in data.iter().enumerate().skip(1) {
        nodes.push(
            start_node(
                LEADER_LOSS_BASE_PORT,
                index,
                vec![local(LEADER_LOSS_BASE_PORT)],
                &root,
                directory.path(),
            )
            .await,
        );
    }
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
    let mut leader = None;
    for (index, node) in nodes.iter().enumerate() {
        if node.council.is_leader().await {
            leader = Some(index);
        }
    }
    let leader = leader.expect("a leader");
    let survivors = [(leader + 1) % 3, (leader + 2) % 3];
    let http = reqwest::Client::new();
    let answer: Value = http
        .post(format!(
            "http://127.0.0.1:{}/v1/batch/array",
            nodes[survivors[0]].api_port
        ))
        .json(&json!({
            "name": "leader-loss",
            "template": { "runtime":"process","exec": BINARY, "command": ["{index}"] },
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

    // The leader is a worker too: kill it once it is running its own share.
    let leader_name = format!("ta{leader}");
    wait_until(
        "the leader holds work",
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
                            node["node"] == leader_name.as_str()
                                && node["counters"]["succeeded"].as_u64().unwrap_or(0) > 0
                        })
                })
        },
    )
    .await;
    nodes[leader].shutdown.cancel();

    // The new leader starts its silence clock at its election, so for the
    // first few seconds its view still shows what the old leader held.
    let mut new_leader = None;
    wait_until("a new leader", Duration::from_secs(30), async || {
        for index in survivors {
            if nodes[index].council.is_leader().await {
                new_leader = Some(index);
            }
        }
        new_leader.is_some()
    })
    .await;
    let new_leader = new_leader.unwrap();
    let lost = reliaburger::meat::NodeId::new(&leader_name);
    let held_at_loss = nodes[new_leader]
        .council
        .desired_state()
        .await
        .task_arrays
        .get(batch_id)
        .and_then(|record| record.state.held_by(&lost).cloned())
        .unwrap_or_default();
    let raft_before = nodes[new_leader]
        .metrics_rx
        .borrow()
        .last_log_index
        .unwrap_or(0);

    let mut last = Value::Null;
    wait_until(
        "the array finishes under the new leader",
        Duration::from_secs(180),
        async || match status(&http, &nodes[new_leader], batch_id).await {
            Some(summary) => {
                let done = summary["done"] == true;
                last = summary;
                done
            }
            None => false,
        },
    )
    .await;
    // Nothing lost, nothing counted twice: the dead leader's commands may
    // have finished, but no report of theirs can be accepted any more.
    assert_eq!(last["status"], "Succeeded", "{last}");
    assert_eq!(last["succeeded"], TASKS, "{last}");
    assert_eq!(last["failed"], 0, "{last}");
    assert_eq!(last["not_run"], 0, "{last}");
    assert_eq!(
        last["retried"],
        TASKS / 500,
        "exactly one accepted retry per flaky index: {last}"
    );
    assert_eq!(last["retry_causes"]["exit_code"], TASKS / 500, "{last}");

    // Bounded replay: only the chunks the dead leader held run again, each
    // at a later grant attempt on a survivor.
    let record = nodes[new_leader]
        .council
        .desired_state()
        .await
        .task_arrays
        .get(batch_id)
        .unwrap()
        .clone();
    let chunks = record.state.spec.chunk_count();
    assert_eq!(record.state.done().len(), u64::from(chunks));
    let mut replayed = 0u64;
    for chunk in 0..chunks {
        let grant = record
            .state
            .accepted_grant(reliaburger::meat::task_array::ChunkId(chunk))
            .expect("every chunk has one accepted grant");
        let attempt = grant.attempt;
        // Only a record folded past MAX_ACCEPTED_RANGES drops the owner.
        assert_ne!(
            grant.owner,
            Some(lost.clone()),
            "chunk {chunk} was accepted from the dead leader"
        );
        if attempt > 1 {
            replayed += 1;
            assert!(
                held_at_loss.contains(chunk),
                "chunk {chunk} replayed at attempt {attempt} but the dead leader didn't hold it"
            );
        }
    }
    assert!(
        replayed <= held_at_loss.len(),
        "{replayed} chunks replayed, {} held by the dead leader",
        held_at_loss.len()
    );
    let entries = nodes[new_leader]
        .metrics_rx
        .borrow()
        .last_log_index
        .unwrap_or(0)
        - raft_before;
    eprintln!(
        "cluster_task_arrays: leader lost mid-run; {} chunks held at loss, {replayed} replayed, {entries} Raft entries afterwards",
        held_at_loss.len()
    );
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
            audit_dir: None,
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
        data_root: None,
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
        task_arrays_on_agent_budget: None,
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

/// Move Raft leadership off `leader` and return the index of the node that
/// leads instead.
///
/// openraft 0.9 has no leadership transfer, and one `trigger().elect()` on a
/// follower doesn't guarantee a handover. Right after a cron fire the leader
/// writes the run's next entries, so the campaign can start while the leader
/// holds an entry the candidate hasn't received yet. The leader and the other
/// follower then refuse the candidate's shorter log. Its higher term still
/// deposes the leader, but the election that follows favours the longest
/// log, which is the old leader's, so the old leader wins again. Its
/// elections therefore stay off until another voter leads, and the most
/// up-to-date other voter campaigns every 3 s (longer than the 2 s leader
/// lease) until one does.
async fn hand_over_leadership(nodes: &[WiredNode], leader: usize) -> usize {
    let others: Vec<usize> = (0..nodes.len()).filter(|&i| i != leader).collect();
    nodes[leader].council.raft().runtime_config().elect(false);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut new_leader = None;
    let mut campaigns = 0;
    let term_before = nodes[leader].metrics_rx.borrow().current_term;
    'campaigns: while tokio::time::Instant::now() < deadline {
        let candidate = others
            .iter()
            .copied()
            .max_by_key(|&i| nodes[i].metrics_rx.borrow().last_log_index)
            .unwrap();
        nodes[candidate]
            .council
            .raft()
            .trigger()
            .elect()
            .await
            .unwrap();
        campaigns += 1;
        let round = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < round {
            for &other in &others {
                if nodes[other].council.is_leader().await {
                    new_leader = Some(other);
                    break 'campaigns;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    nodes[leader].council.raft().runtime_config().elect(true);
    let new_leader = new_leader.expect("timed out: replacement cron leader");
    let term = nodes[new_leader].metrics_rx.borrow().current_term;
    eprintln!(
        "handover: node {new_leader} leads at term {term} (was {term_before}) after {campaigns} campaign(s)"
    );
    new_leader
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
    // Registering a schedule marks the minute it lands in as already seen, so
    // a request sent in a minute's last moments could land in the target
    // minute and never fire. Register early in a minute instead.
    let now = time::OffsetDateTime::now_utc();
    if now.second() > 45 {
        tokio::time::sleep(Duration::from_secs(61 - u64::from(now.second()))).await;
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
        .json(&json!({"name":"scheduled-singleton","definition":{"template":{"runtime":"process","exec":BINARY},"cron":{"expression":expression}}})).send().await.unwrap();
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
    // Move leadership with a real election while keeping all workers and
    // quorum alive. Removing a voter is not a handover: the self-healing
    // council may restore it.
    let new_leader = hand_over_leadership(&nodes, leader).await;
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
async fn cron_in_one_namespace_fires_while_another_fills_the_bulk_cap() {
    use reliaburger::meat::task_array_store::{
        MAX_ACTIVE_ARRAYS, MAX_ACTIVE_RUNS_PER_NAMESPACE, RESERVED_TRIGGERED_RUNS,
    };
    assert!(cluster_tests_enabled());
    // Every task takes far longer than the test, so every array stays active.
    let (root, _data, nodes, token) = common_cluster(Duration::from_secs(600)).await;
    let leader = nodes
        .iter()
        .position(|node| *node.thinks_leader.borrow())
        .unwrap();
    let follower = (leader + 1) % 3;
    let http = reqwest::Client::new();
    let api = |index: usize| format!("http://127.0.0.1:{}", nodes[index].api_port);

    // The bulk submissions below take a few seconds and must all land before
    // the occurrence fires, or it takes one of their slots. Skip to the minute
    // after next when the next one is less than 30 seconds away.
    let now = time::OffsetDateTime::now_utc();
    let next = now + time::Duration::minutes(if now.second() >= 30 { 2 } else { 1 });
    let expression = format!(
        "{} {} {} {} *",
        next.minute(),
        next.hour(),
        next.day(),
        u8::from(next.month())
    );
    let response = http
        .post(format!("{}/v1/jobs/runs", api(follower)))
        .bearer_auth(&token)
        .json(&json!({"name":"nightly","namespace":"ops","definition":{"template":{"runtime":"process","exec":BINARY},"cron":{"expression":expression}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);

    // Other tenants fill every bulk slot before the occurrence comes round.
    let bulk = MAX_ACTIVE_ARRAYS - RESERVED_TRIGGERED_RUNS;
    let submit = async |name: String, namespace: String| {
        http.post(format!("{}/v1/batch/array", api(leader)))
            .bearer_auth(&token)
            .json(&json!({"name":name,"namespace":namespace,"template":{"runtime":"process","exec":BINARY},"spec":{"count":1}}))
            .send()
            .await
            .unwrap()
    };
    for i in 0..bulk {
        let namespace = format!("tenant-{}", i / MAX_ACTIVE_RUNS_PER_NAMESPACE);
        let response = submit(format!("bulk-{i}"), namespace).await;
        let status = response.status();
        assert_eq!(
            status,
            202,
            "bulk array {i}: {}",
            response.text().await.unwrap_or_default()
        );
    }
    let refused = submit("one-more".into(), "tenant-late".into()).await;
    assert!(refused.status().is_client_error(), "{}", refused.status());
    let reason = refused.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(reason.contains("cron fires and deployments"), "{reason}");

    wait_until(
        "the cron occurrence admitted into the reserved headroom",
        Duration::from_secs(135),
        async || {
            nodes[leader]
                .council
                .desired_state()
                .await
                .task_arrays
                .jobs()
                .runs()
                .any(|(_, run)| run.name == "nightly" && run.namespace == "ops")
        },
    )
    .await;
    let arrays = nodes[leader].council.desired_state().await.task_arrays;
    let record = arrays.jobs().definition("ops", "nightly").unwrap();
    assert_eq!(record.skipped, None, "the fire must run, not be skipped");
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
        .json(&json!({"name":"side-effects","request_id":"first","definition":{"template":{"runtime":"process","exec":BINARY},"tasks":{"max_attempts":1},"replay_unknown":false}})).send().await.unwrap();
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
            .owner
            .unwrap()
            .0,
        owner
    );
    root.cancel();
}

/// Ports one wired node binds: gossip, Raft, reporting and the API.
const HUNG_PORTS_PER_NODE: u16 = 4;
const HUNG_ARRAYS: usize = 16;
// 16 arrays of 4,000 tasks at 20 ms each on 3 × 16 slots take at least
// 27 s however fast the host is: three times the ~9 s from submitting them
// to the end of the hung window, and 13 s less than 6,000 tasks took (#760).
const HUNG_TASKS: u32 = 4_000;
const HUNG_SYNC_INTERVAL: Duration = Duration::from_millis(200);

async fn start_hung_node(
    index: usize,
    ports: &PortBlockReservation,
    seeds: Vec<std::net::SocketAddr>,
    root: &CancellationToken,
    data: &std::path::Path,
) -> WiredNode {
    let executor = TaskArrayNode::new(
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
            audit_dir: None,
        },
        // Slow enough that sixteen arrays are all still running while one
        // worker hangs.
        NodeRunner::Fake(FakeRunner::new(Duration::from_millis(20), |_| {
            AttemptOutcome::Exited { code: 0 }
        })),
    );
    start_wired_node(WiredNodeOptions {
        name: format!("hung{index}"),
        gossip_port: ports.base,
        seeds,
        shutdown: root.child_token(),
        data_dir_prefix: "rb-hung-worker",
        data_root: None,
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
        task_arrays: Some(Arc::new(TaskArrayService::with_timings(
            Some(Arc::new(executor)),
            HUNG_SYNC_INTERVAL,
            Duration::from_secs(3),
        ))),
        task_arrays_on_agent_budget: None,
    })
    .await
}

/// Hang a worker's sync API without stopping its gossip: swap its control
/// fence file for a FIFO with no writer, so its next sync blocks reading it
/// (as on a stuck disk). Retries if a sync in flight renames a fresh fence
/// over the FIFO.
async fn hang_sync(control: &std::path::Path) {
    use std::os::unix::fs::FileTypeExt;
    for _ in 0..50 {
        let _ = std::fs::remove_file(control);
        nix::unistd::mkfifo(control, nix::sys::stat::Mode::S_IRWXU).unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        if std::fs::metadata(control).is_ok_and(|meta| meta.file_type().is_fifo()) {
            return;
        }
    }
    panic!("the worker kept replacing its fence file");
}

/// Release every hung sync. The leader gives up on a sync after its 3 s
/// timeout and sends another, so by now two or more of the worker's syncs
/// can be blocked on the FIFO, each holding a blocking thread.
///
/// First move the FIFO aside, so the next sync finds no fence file and
/// carries on. Then open and close its write end until no reader holds it.
/// An open wakes the readers blocked opening the FIFO, and a close lets
/// their reads see end of file. One open and close frees every reader on
/// Linux, but on macOS some readers stay blocked in `read` after it, and
/// the runtime then waits for their threads forever at shutdown (#760).
/// No reader is left once opening the write end without blocking fails
/// with `ENXIO` a few times in a row.
async fn release_sync(control: std::path::PathBuf) {
    use std::os::unix::fs::OpenOptionsExt;
    tokio::task::spawn_blocking(move || {
        let aside = control.with_extension("released");
        std::fs::rename(&control, &aside).unwrap();
        let mut woke = false;
        let mut unheld = 0;
        for _ in 0..200 {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
                .open(&aside)
            {
                Ok(writer) => {
                    woke = true;
                    unheld = 0;
                    drop(writer);
                }
                Err(error) if error.raw_os_error() == Some(nix::libc::ENXIO) => {
                    unheld += 1;
                    if woke && unheld >= 3 {
                        std::fs::remove_file(&aside).unwrap();
                        return;
                    }
                }
                Err(error) => panic!("can't open the FIFO's write end: {error}"),
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("readers still hold the FIFO, or none was ever blocked on it");
    })
    .await
    .unwrap();
}

/// Successful tasks the leader has heard about from `node`, in a status.
fn node_succeeded(summary: &Value, node: &str) -> Option<u64> {
    summary["nodes"]
        .as_array()?
        .iter()
        .find(|view| view["node"] == node)?["counters"]["succeeded"]
        .as_u64()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn hung_worker_api_does_not_stall_other_nodes_arrays() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );
    let root = CancellationToken::new();
    let data: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    // Leased blocks, held until the test ends, so no other fixture on the
    // host binds these ports while the cluster runs.
    let ports: Vec<PortBlockReservation> = (0..3)
        .map(|_| reserve_port_block_lease(HUNG_PORTS_PER_NODE))
        .collect();
    let seed = local(ports[0].base);
    let mut nodes = vec![start_hung_node(0, &ports[0], vec![], &root, data[0].path()).await];
    for (index, directory) in data.iter().enumerate().skip(1) {
        nodes
            .push(start_hung_node(index, &ports[index], vec![seed], &root, directory.path()).await);
    }
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
    let mut leader = None;
    for (index, node) in nodes.iter().enumerate() {
        if node.council.is_leader().await {
            leader = Some(index);
        }
    }
    let leader = leader.expect("a leader");
    let healthy = format!("hung{}", (leader + 1) % 3);
    let victim = (leader + 2) % 3;
    let victim_name = format!("hung{victim}");

    let http = reqwest::Client::new();
    let mut batches = Vec::new();
    for i in 0..HUNG_ARRAYS {
        let answer: Value = http
            .post(format!(
                "http://127.0.0.1:{}/v1/batch/array",
                nodes[leader].api_port
            ))
            .json(&json!({
                "name": format!("busy-{i}"),
                "template": { "runtime":"process","exec": BINARY, "command": ["{index}"] },
                "spec": { "count": HUNG_TASKS, "chunk_size": 64 },
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        batches.push(
            answer["batch_id"]
                .as_u64()
                .unwrap_or_else(|| panic!("{answer}")),
        );
    }
    wait_until(
        "every node runs work",
        Duration::from_secs(60),
        async || match status(&http, &nodes[leader], batches[0]).await {
            Some(summary) => [healthy.as_str(), victim_name.as_str()]
                .iter()
                .all(|node| node_succeeded(&summary, node).is_some_and(|count| count > 0)),
            None => false,
        },
    )
    .await;

    // Sixteen busy arrays, one Raft entry per tick between them.
    let window = Duration::from_secs(2);
    let before = nodes[leader]
        .metrics_rx
        .borrow()
        .last_log_index
        .unwrap_or(0);
    tokio::time::sleep(window).await;
    let after = nodes[leader]
        .metrics_rx
        .borrow()
        .last_log_index
        .unwrap_or(0);
    let ticks = window.as_millis() / HUNG_SYNC_INTERVAL.as_millis();
    let entries = u128::from(after - before);
    eprintln!(
        "hung_worker: {HUNG_ARRAYS} arrays, {entries} Raft entries in {window:?} ({ticks} ticks)"
    );
    assert!(
        entries <= ticks + 2,
        "{entries} Raft entries in {ticks} ticks for {HUNG_ARRAYS} arrays"
    );

    let control = data[victim].path().join("task-arrays/control.json");
    hang_sync(&control).await;

    // While the victim's API hangs, the leader still hears from the healthy
    // node every tick: what it has run, summed over every array, keeps
    // moving several times a second, not once per sync timeout.
    let mut seen = BTreeSet::new();
    let started = tokio::time::Instant::now();
    while started.elapsed() < Duration::from_secs(4) {
        let mut total = 0;
        for batch_id in &batches {
            total += status(&http, &nodes[leader], *batch_id)
                .await
                .and_then(|summary| node_succeeded(&summary, &healthy))
                .unwrap_or(0);
        }
        seen.insert(total);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let listed = nodes[leader].membership_table.read().await.len();
    // Release before asserting: a reader still blocked on the FIFO would
    // keep the test's runtime from shutting down after a failure.
    release_sync(control).await;
    eprintln!(
        "hung_worker: the healthy node's view changed {} times in 4 s",
        seen.len()
    );
    // Waiting on the hung node's 3 s sync timeout every tick would allow at
    // most two fresh readings in 4 s. The healthy node's own syncs can be
    // slow on a busy host (each fsyncs its control fence), so don't demand
    // all twenty ticks.
    assert!(
        seen.len() >= 5,
        "the healthy node was heard {} times in 4 s",
        seen.len()
    );
    assert_eq!(listed, 3, "gossip still lists the hung node");

    for batch_id in batches {
        let mut last = Value::Null;
        wait_until(
            "every array finishes",
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
        assert_eq!(last["succeeded"], HUNG_TASKS, "{last}");
    }
    root.cancel();
}

/// A node whose task-array executor shares its agent's execution budget, as
/// Bun wires it. `executor: false` leaves the node without one, so arrays
/// run only on the nodes that have it.
async fn shared_budget_node(
    index: usize,
    seeds: Vec<std::net::SocketAddr>,
    root: &CancellationToken,
    data: &std::path::Path,
    executor: bool,
) -> WiredNode {
    let directory = data.join("task-arrays");
    let build: Option<cluster_support::TaskArraysOnBudget> = executor.then(|| {
        Box::new(move |budget| {
            let node = TaskArrayNode::new(
                TaskArrayNodeConfig {
                    root: directory,
                    policy: ProcessWorkloadsConfig {
                        allowed_binaries: vec![BINARY.into()],
                        mount_isolation: false,
                        ..Default::default()
                    },
                    default_concurrency: 8,
                    backoff: (Duration::from_millis(1), Duration::from_millis(5)),
                    group_commit: GroupCommit::default(),
                    audit_dir: None,
                },
                // Each attempt holds its lease long enough to fill the node.
                NodeRunner::Fake(FakeRunner::new(Duration::from_secs(120), |_| {
                    AttemptOutcome::Exited { code: 0 }
                })),
            )
            .with_budget(budget);
            Arc::new(TaskArrayService::with_timings(
                Some(Arc::new(node)),
                Duration::from_millis(100),
                Duration::from_secs(10),
            ))
        }) as cluster_support::TaskArraysOnBudget
    });
    start_wired_node(WiredNodeOptions {
        name: format!("shared{index}"),
        gossip_port: BASE_PORT + 200 + index as u16 * 10,
        seeds,
        shutdown: root.child_token(),
        data_dir_prefix: "rb-shared-budget",
        stale_report_timeout_secs: 10,
        metrics_rollup: None,
        scheduler: Some(reliaburger::config::node::ReconstructionSection {
            report_threshold_percent: 95,
            learning_period_timeout_secs: 2,
            large_cluster_timeout_secs: 4,
            large_cluster_node_count: 5000,
        }),
        lease_reaper: false,
        membership: MembershipSource::Gossip,
        service_identity: Some(SERVICE_TOKEN.into()),
        operator_token: None,
        fault_injection: false,
        labels: Default::default(),
        keep_data_dir: false,
        data_root: None,
        task_arrays: Some(Arc::new(TaskArrayService::with_timings(
            None,
            Duration::from_millis(100),
            Duration::from_secs(10),
        ))),
        task_arrays_on_agent_budget: build,
    })
    .await
}

/// Task attempts and app replicas lease from one budget per node. With one
/// node's budget full of an array's attempts, the leader places an app's
/// replicas on the other two: the full node's state report carries its job
/// reservations, and the leader counts them as allocated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a real multi-node cluster; run with make test-cluster"]
async fn app_replica_lands_on_the_idle_node_while_another_runs_an_array() {
    assert!(cluster_tests_enabled());
    let root = CancellationToken::new();
    let data: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut nodes = Vec::new();
    for (index, directory) in data.iter().enumerate() {
        let seeds = if index == 0 {
            vec![]
        } else {
            vec![local(BASE_PORT + 200)]
        };
        nodes.push(shared_budget_node(index, seeds, &root, directory.path(), index == 0).await);
    }
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
    let leader = nodes
        .iter()
        .position(|node| *node.thinks_leader.borrow())
        .expect("a leader");

    // Fill shared0's 8000m with eight 1000m attempts that sleep.
    let http = reqwest::Client::new();
    let answer: Value = http
        .post(format!(
            "http://127.0.0.1:{}/v1/batch/array",
            nodes[leader].api_port
        ))
        .json(&json!({
            "name": "filler",
            "template": { "runtime": "process", "exec": BINARY, "cpu": "1000m" },
            "spec": { "count": 64 },
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(answer["batch_id"].is_u64(), "{answer}");
    wait_until(
        "the leader sees shared0's budget held by jobs",
        Duration::from_secs(60),
        async || {
            nodes[leader]
                .aggregated_rx
                .borrow()
                .reports
                .get(&reliaburger::meat::NodeId::new("shared0"))
                .is_some_and(|report| report.job_reservations.cpu_millicores >= 8000)
        },
    )
    .await;

    let app = reliaburger::meat::AppId::new("web", "default");
    nodes[leader]
        .council
        .write(reliaburger::council::RaftRequest::AppSpec {
            app_id: app.clone(),
            spec: Box::new(
                toml::from_str(
                    "image = 'proc-grill:image-ignored'\ncommand = ['sleep', '600']\ncpu = '1000m'\nreplicas = 3",
                )
                .unwrap(),
            ),
        })
        .await
        .unwrap();
    let mut placed = Vec::new();
    wait_until("the app is placed", Duration::from_secs(60), async || {
        placed = nodes[leader]
            .council
            .desired_state()
            .await
            .scheduling
            .get(&app)
            .map(|placements| {
                placements
                    .iter()
                    .map(|placement| placement.node_id.0.clone())
                    .collect()
            })
            .unwrap_or_default();
        placed.len() == 3
    })
    .await;
    root.cancel();
    assert!(
        placed.iter().all(|node| node != "shared0"),
        "a replica landed on the node jobs had filled: {placed:?}"
    );
}
