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

    let next = time::OffsetDateTime::now_utc() + time::Duration::minutes(1);
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
        assert_eq!(response.status(), 202, "bulk array {i}");
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
        Duration::from_secs(75),
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

const HUNG_BASE_PORT: u16 = BASE_PORT + 300;
const HUNG_ARRAYS: usize = 16;
const HUNG_TASKS: u32 = 6_000;
const HUNG_SYNC_INTERVAL: Duration = Duration::from_millis(200);

async fn start_hung_node(
    index: usize,
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
        },
        // Slow enough that sixteen arrays are all still running while one
        // worker hangs.
        NodeRunner::Fake(FakeRunner::new(Duration::from_millis(20), |_| {
            AttemptOutcome::Exited { code: 0 }
        })),
    );
    start_wired_node(WiredNodeOptions {
        name: format!("hung{index}"),
        gossip_port: HUNG_BASE_PORT + (index as u16) * 10,
        seeds,
        shutdown: root.child_token(),
        data_dir_prefix: "rb-hung-worker",
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

/// Release the hung sync: open the FIFO's write end (which lets the blocked
/// reader's open finish), remove it, then close it so the read sees end of
/// file. The node's next sync finds no fence file and carries on.
async fn release_sync(control: std::path::PathBuf) {
    use std::os::unix::fs::OpenOptionsExt;
    tokio::task::spawn_blocking(move || {
        for _ in 0..100 {
            if let Ok(writer) = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
                .open(&control)
            {
                std::fs::remove_file(&control).unwrap();
                drop(writer);
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("nothing was blocked on the FIFO");
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
    let mut nodes = vec![start_hung_node(0, vec![], &root, data[0].path()).await];
    for (index, directory) in data.iter().enumerate().skip(1) {
        nodes.push(
            start_hung_node(index, vec![local(HUNG_BASE_PORT)], &root, directory.path()).await,
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
