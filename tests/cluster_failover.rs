//! Phase 12b.2 tier acceptance: an 8+ node cluster reconciles and reports
//! through leader failover (H1/D1, CP1, CP5).
//!
//! Nine fully wired in-process nodes (gossip + Raft + reporting + a real
//! BunAgent on ProcessGrill + HTTP API + leader scheduler + placement
//! reconciler — the exact `bun --cluster` wiring). The council caps at
//! seven voters, so two nodes are genuine workers with no Raft view of the
//! leader: everything they know arrives through the gossip directory. The
//! test proves that (a) every node — voter or worker — reports to the
//! leader, (b) a daemonset app converges onto the workers, and (c) killing
//! the leader re-points every survivor at the new leader, whose aggregator
//! recovers full coverage in a fresh epoch and still reconciles placements.
//!
//! Gated behind `RELIABURGER_CLUSTER_TESTS=1` (the
//! `RELIABURGER_UPGRADE_TESTS` precedent): nine nodes with seven-voter Raft
//! elections are far past a 2-core CI budget. Run via
//! `RELIABURGER_CLUSTER_TESTS=1 cargo test --test cluster_failover`.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use reliaburger::cluster::orchestrate::spawn_placement_reconciler;
use reliaburger::council::types::RaftRequest;
use reliaburger::meat::{AppId, NodeId};
use reliaburger::reporting::aggregator::AggregatedState;

#[path = "support/cluster.rs"]
mod cluster_support;
use cluster_support::{
    MembershipSource, WiredNode, WiredNodeOptions, cluster_tests_enabled, local, start_wired_node,
};

const RETIREMENT_SERVICE_TOKEN: &str = "cluster-test-retirement-service";
const NODE_COUNT: usize = 9;
const BASE_PORT: u16 = 19510;

impl WiredNode {
    /// Resolve a service by name against this node's agent — the merged
    /// local + cluster-catalogue view (12b.4).
    async fn resolve(&self, app: &str) -> Option<reliaburger::onion::types::ResolveResponse> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(reliaburger::bun::agent::AgentCommand::Resolve {
                app_name: app.to_string(),
                response: tx,
            })
            .await
            .ok()?;
        rx.await.ok().flatten()
    }

    async fn is_leader(&self) -> bool {
        self.council.is_leader().await
    }

    fn voter_count(&self) -> usize {
        self.metrics_rx
            .borrow()
            .membership_config
            .membership()
            .voter_ids()
            .count()
    }

    fn voter_ids(&self) -> BTreeSet<u64> {
        self.metrics_rx
            .borrow()
            .membership_config
            .membership()
            .voter_ids()
            .collect()
    }
}

/// The voter set that every listed member has applied, once they all agree.
///
/// A node's Raft metrics show a membership entry as soon as it is appended,
/// before it commits, and a joint configuration reports the union of its old
/// and new voters. So the leader can show three voters while it is still
/// half-way from `{a, b}` to `{a, b, c}`. Killing it then leaves the
/// survivors on a joint configuration whose old half needs the dead leader,
/// and no election can succeed. Only a uniform configuration which every
/// survivor has applied (so it is committed) makes a leader kill safe.
fn settled_voters(nodes: &[&WiredNode]) -> Option<BTreeSet<u64>> {
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
            Some((agreed_id, agreed_voters))
                if *agreed_id == log_id && *agreed_voters == voters => {}
            Some(_) => return None,
        }
    }
    agreed.map(|(_, voters)| voters)
}

/// Start one fully wired node: the same subsystems `bun --cluster` runs.
async fn start_node(index: usize, seeds: Vec<SocketAddr>, root: &CancellationToken) -> WiredNode {
    start_node_with_scheduler(index, seeds, root, true).await
}

async fn start_node_with_scheduler(
    index: usize,
    seeds: Vec<SocketAddr>,
    root: &CancellationToken,
    schedule: bool,
) -> WiredNode {
    start_node_for_test(index, seeds, root, schedule, None).await
}

async fn start_node_for_test(
    index: usize,
    seeds: Vec<SocketAddr>,
    root: &CancellationToken,
    schedule: bool,
    operator: Option<reliaburger::sesame::types::ApiToken>,
) -> WiredNode {
    start_wired_node(WiredNodeOptions {
        name: format!("fo{index}"),
        gossip_port: BASE_PORT + (index as u16) * 10,
        seeds,
        // Cancelling a node's own token kills that node only.
        shutdown: root.child_token(),
        data_dir_prefix: "rb-failover",
        stale_report_timeout_secs: 10,
        metrics_rollup: None,
        scheduler: schedule.then_some(reliaburger::config::node::ReconstructionSection {
            report_threshold_percent: 80,
            learning_period_timeout_secs: 5,
            large_cluster_timeout_secs: 10,
            large_cluster_node_count: 5000,
        }),
        lease_reaper: false,
        // The two worker nodes have no Raft view, so peers come from the
        // gossip directory.
        membership: MembershipSource::Directory,
        service_identity: Some(RETIREMENT_SERVICE_TOKEN.into()),
        operator_token: operator,
        fault_injection: false,
    })
    .await
}

async fn wait_until(what: &str, timeout: Duration, mut cond: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond().await {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Node names whose report (in `state`) lists a running `app`.
fn nodes_reporting_app(state: &AggregatedState, app: &str) -> usize {
    state
        .reports
        .values()
        .filter(|report| report.running_apps.iter().any(|a| a.app_name == app))
        .count()
}

fn daemonset_spec(command_seconds: u32) -> reliaburger::config::app::AppSpec {
    toml::from_str(&format!(
        r#"
        image = "proc-grill:image-ignored"
        command = ["sleep", "{command_seconds}"]
        replicas = "*"
        "#
    ))
    .unwrap()
}

/// A single-replica app with a port, so it lands on exactly one node and
/// becomes a resolvable service with a backend.
fn ported_service_spec(name_port: u16) -> reliaburger::config::app::AppSpec {
    toml::from_str(&format!(
        r#"
        image = "proc-grill:image-ignored"
        command = ["sleep", "600"]
        replicas = 1
        port = {name_port}
        "#
    ))
    .unwrap()
}

/// Which node (by name) currently reports running `app`, if any.
fn node_running_app(state: &AggregatedState, app: &str) -> Option<String> {
    state
        .reports
        .iter()
        .find(|(_, report)| report.running_apps.iter().any(|a| a.app_name == app))
        .map(|(node_id, _)| node_id.0.clone())
}

/// 12b.4: a service deployed on node B resolves + routes from node A. Every
/// node overlays the leader's replicated endpoint catalogue onto its local
/// service map, so cross-node resolution works even though the backend runs
/// elsewhere. Also proves the catalogue survives a leader change.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host"]
async fn service_on_one_node_resolves_and_survives_leader_change_from_another() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );

    let root = CancellationToken::new();
    // Three nodes, all voters (cap is seven): killing the leader leaves a
    // two-node quorum, so a survivor takes over cleanly. Three keeps the
    // election light while still proving cross-node resolve + failover.
    const N: usize = 3;
    let mut nodes = Vec::with_capacity(N);
    nodes.push(start_node(100, vec![], &root).await);
    for index in 101..(100 + N) {
        nodes.push(start_node(index, vec![local(BASE_PORT + 1000)], &root).await);
    }

    // Wait for the council to commit all three as voters before deploying:
    // killing the leader before the voter set settles can leave the survivors
    // without a committed quorum, and election stalls.
    wait_until(
        "three settled voters",
        Duration::from_secs(60),
        async || {
            settled_voters(&nodes.iter().collect::<Vec<_>>())
                .is_some_and(|voters| voters.len() == N)
        },
    )
    .await;
    wait_until(
        "all nodes reporting to the leader",
        Duration::from_secs(60),
        async || nodes[0].aggregated_rx.borrow().reports.len() == N,
    )
    .await;

    // Deploy one replica of a ported service via the leader.
    assert!(nodes[0].is_leader().await, "bootstrap node should lead");
    nodes[0]
        .council
        .write(RaftRequest::AppSpec {
            app_id: AppId::new("redis", "default"),
            spec: Box::new(ported_service_spec(6379)),
        })
        .await
        .unwrap();

    // Wait until exactly one node reports it running.
    wait_until(
        "redis running on some node",
        Duration::from_secs(90),
        async || node_running_app(&nodes[0].aggregated_rx.borrow(), "redis").is_some(),
    )
    .await;
    let host_node = node_running_app(&nodes[0].aggregated_rx.borrow(), "redis").unwrap();
    eprintln!("redis landed on {host_node}");

    // Pick a DIFFERENT node and resolve the service from it. The catalogue is
    // replicated + polled, so this node reaches redis though it runs elsewhere.
    let other = nodes
        .iter()
        .find(|n| n.name != host_node)
        .expect("a node other than the host");
    wait_until(
        "redis resolves with a healthy backend from another node",
        Duration::from_secs(60),
        async || match other.resolve("redis").await {
            Some(info) => info.healthy_backends >= 1,
            None => false,
        },
    )
    .await;
    let info = other.resolve("redis").await.expect("resolves");
    assert_eq!(info.app_name, "redis");
    assert_eq!(info.port, 6379);
    assert!(info.total_backends >= 1, "cross-node backend missing");

    // Kill the leader; a survivor takes over. The replicated catalogue must
    // survive the leader change: the same other-node resolution still works.
    let old_leader = nodes[0].name.clone();
    nodes[0].shutdown.cancel();
    nodes[0].council.shutdown().await.ok();

    let survivors: Vec<&WiredNode> = nodes.iter().filter(|n| n.name != old_leader).collect();
    let mut new_leader: Option<&WiredNode> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    while new_leader.is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no new leader elected"
        );
        for node in &survivors {
            if node.is_leader().await {
                new_leader = Some(node);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // A surviving node that isn't the (possibly-relocated) host still resolves
    // redis from the post-failover catalogue.
    let resolver = survivors
        .iter()
        .find(|n| n.name != old_leader)
        .expect("a surviving resolver");
    wait_until(
        "redis still resolves after the leader change",
        Duration::from_secs(90),
        async || match resolver.resolve("redis").await {
            Some(info) => info.total_backends >= 1,
            None => false,
        },
    )
    .await;

    root.cancel();
    tokio::time::sleep(Duration::from_millis(500)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host"]
async fn eight_plus_node_cluster_reconciles_and_reports_through_leader_failover() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );

    let root = CancellationToken::new();
    let mut nodes = Vec::with_capacity(NODE_COUNT);
    nodes.push(start_node(0, vec![], &root).await);
    for index in 1..NODE_COUNT {
        nodes.push(start_node(index, vec![local(BASE_PORT)], &root).await);
    }

    // The council reconciler grows the voter set to its cap of seven; the
    // remaining two nodes stay workers — the H1 subjects.
    wait_until("seven voters", Duration::from_secs(60), async || {
        nodes[0].voter_count() == 7
    })
    .await;

    // (a) EVERY node reports to the leader — including the two outside the
    // council, which can only have found it through the gossip directory.
    wait_until(
        "all 9 nodes reporting to the bootstrap leader",
        Duration::from_secs(60),
        async || nodes[0].aggregated_rx.borrow().reports.len() == NODE_COUNT,
    )
    .await;

    // The leader's committed membership is authoritative. Individual nodes'
    // metrics watches can trail that commit briefly, so counting each node's
    // local opinion here produced a false third "worker" in CI.
    let voter_ids = nodes[0].voter_ids();
    let workers: Vec<&WiredNode> = nodes
        .iter()
        .filter(|node| !voter_ids.contains(&node.raft_id))
        .collect();
    assert_eq!(workers.len(), 2, "expected exactly two non-voter workers");
    for worker in &workers {
        assert!(
            nodes[0]
                .aggregated_rx
                .borrow()
                .reports
                .contains_key(&NodeId::new(&worker.name)),
            "non-voter {} must appear in the leader's aggregated reports",
            worker.name
        );
    }

    // (b) A daemonset app converges everywhere — on the workers too, which
    // requires their placement reconcilers to reach the leader's API.
    assert!(nodes[0].is_leader().await, "bootstrap node should lead");
    nodes[0]
        .council
        .write(RaftRequest::AppSpec {
            app_id: AppId::new("web", "default"),
            spec: Box::new(daemonset_spec(600)),
        })
        .await
        .unwrap();

    wait_until(
        "web running on all 9 nodes (per aggregated reports)",
        Duration::from_secs(90),
        async || nodes_reporting_app(&nodes[0].aggregated_rx.borrow(), "web") == NODE_COUNT,
    )
    .await;

    // (c) Kill the leader. A new leader must emerge, every survivor must
    // re-point its reporting and placement polling at it (without restart),
    // and coverage must recover in the new epoch — pre-failover reports
    // cannot satisfy it (CP5), so what we see below is all fresh truth.
    let old_leader_name = nodes[0].name.clone();
    nodes[0].shutdown.cancel();
    // The cancellation token stops the node's spawned tasks (RPC server,
    // gossip, reporting), but the in-process Raft core only dies with an
    // explicit shutdown — a real `kill -9` takes both out at once.
    nodes[0].council.shutdown().await.ok();

    let survivors: Vec<&WiredNode> = nodes.iter().filter(|n| n.name != old_leader_name).collect();

    let mut new_leader: Option<&WiredNode> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while new_leader.is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no new leader elected after the old one was killed"
        );
        for node in &survivors {
            if node.is_leader().await {
                new_leader = Some(node);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let new_leader = new_leader.unwrap();
    eprintln!("new leader: {}", new_leader.name);

    // Reporting coverage recovers on the NEW leader: all 8 survivors,
    // still running their daemonset instances.
    wait_until(
        "all 8 survivors reporting to the new leader with web running",
        Duration::from_secs(90),
        async || {
            let state = new_leader.aggregated_rx.borrow().clone();
            state.reports.len() == NODE_COUNT - 1
                && nodes_reporting_app(&state, "web") == NODE_COUNT - 1
        },
    )
    .await;

    // Placements still reconcile through the new leader: a fresh app
    // lands on every survivor, workers included.
    new_leader
        .council
        .write(RaftRequest::AppSpec {
            app_id: AppId::new("web2", "default"),
            spec: Box::new(daemonset_spec(600)),
        })
        .await
        .unwrap();

    wait_until(
        "web2 running on all 8 survivors after failover",
        Duration::from_secs(90),
        async || nodes_reporting_app(&new_leader.aggregated_rx.borrow(), "web2") == NODE_COUNT - 1,
    )
    .await;

    root.cancel();
    // Give agents a moment to stop their sleep processes.
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// Ownership survives a real leader change even when the worker's local
/// placement journal is missing and it has missed the deletion entirely.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host"]
async fn lease_retirement_waits_for_paused_worker_across_leader_change() {
    use reliaburger::meat::{Placement, Resources, SchedulingDecision};
    use reliaburger::testkit::lease::{
        LeaseError, TestLease, cleanup_cluster_lease, now_unix_millis,
    };
    assert!(cluster_tests_enabled());
    let root = CancellationToken::new();
    let mut nodes = Vec::new();
    nodes.push(start_node_with_scheduler(200, vec![], &root, false).await);
    for index in 201..203 {
        nodes.push(
            start_node_with_scheduler(index, vec![local(BASE_PORT + 2000)], &root, false).await,
        );
    }
    wait_until(
        "three committed voters",
        Duration::from_secs(60),
        async || {
            settled_voters(&nodes.iter().collect::<Vec<_>>())
                .is_some_and(|voters| voters.len() == 3)
        },
    )
    .await;
    let now = now_unix_millis();
    let lease = TestLease::new(
        "failover".into(),
        "operator".into(),
        "operator".into(),
        "rbtest-failover".into(),
        now,
        now + 600_000,
    )
    .unwrap();
    let app_id = AppId::new("cleanup", &lease.namespace);
    let mut spec = ported_service_spec(8181);
    spec.namespace = Some(lease.namespace.clone());
    for request in [
        RaftRequest::TestLeaseCreate(lease.clone()),
        RaftRequest::TestLeaseAppSpec {
            lease_id: lease.lease_id.clone(),
            observed_at_unix_ms: now,
            app_id: app_id.clone(),
            spec: Box::new(spec),
        },
        RaftRequest::SchedulingDecision(SchedulingDecision {
            app_id: app_id.clone(),
            placements: vec![Placement {
                node_id: NodeId::new(&nodes[2].name),
                resources: Resources::new(500, 1024 * 1024, 0),
            }],
        }),
    ] {
        assert!(!matches!(
            nodes[0].council.write(request).await.unwrap(),
            reliaburger::council::CouncilResponse::Refused { .. }
        ));
    }
    wait_until(
        "worker running leased process",
        Duration::from_secs(30),
        async || {
            nodes[2]
                .resolve("cleanup")
                .await
                .is_some_and(|view| view.total_backends == 1)
        },
    )
    .await;
    nodes[2].reconciler.abort();
    let _ = (&mut nodes[2].reconciler).await;
    assert!(matches!(
        cleanup_cluster_lease(&nodes[0].council, "failover", None).await,
        Err(LeaseError::CleanupPending)
    ));
    let client = reqwest::Client::new();
    // A follower must never turn its possibly stale empty view into an
    // authoritative instruction to retire local work.
    assert_eq!(
        client
            .get(format!(
                "http://127.0.0.1:{}/v1/placements/{}",
                nodes[1].api_port, nodes[2].name
            ))
            .bearer_auth(RETIREMENT_SERVICE_TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    nodes[0].shutdown.cancel();
    nodes[0].council.shutdown().await.unwrap();
    let mut leader = None;
    wait_until("successor leader", Duration::from_secs(60), async || {
        for (index, node) in nodes.iter().enumerate().skip(1) {
            if node.is_leader().await {
                leader = Some(index);
                return true;
            }
        }
        false
    })
    .await;
    let leader = leader.unwrap();
    assert!(matches!(
        cleanup_cluster_lease(&nodes[leader].council, "failover", None).await,
        Err(LeaseError::CleanupPending)
    ));
    assert_eq!(
        nodes[leader].council.desired_state().await.test_leases["failover"]
            .placements
            .len(),
        1
    );
    // A fresh checkpoint cannot hide the still-live runtime from the leader's
    // retirement instruction. No local inventory is available on this restart.
    let checkpoint = tempfile::tempdir().unwrap();
    let resumed = spawn_placement_reconciler(
        nodes[2].name.clone(),
        nodes[2].metrics_rx.clone(),
        nodes[2].directory_rx.clone(),
        2,
        Some(RETIREMENT_SERVICE_TOKEN.into()),
        nodes[2].cmd_tx.clone(),
        nodes[2].shutdown.clone(),
        reliaburger::cluster::ClusterHttp::plaintext(),
        Some(checkpoint.path().into()),
        reliaburger::config::node::RuntimeSection::default().stop_confirmation_timeout(),
    );
    wait_until(
        "confirmed worker retirement",
        Duration::from_secs(30),
        async || {
            nodes[leader].council.desired_state().await.test_leases["failover"]
                .placements
                .is_empty()
        },
    )
    .await;
    assert!(
        nodes[2]
            .resolve("cleanup")
            .await
            .is_none_or(|view| view.total_backends == 0)
    );
    cleanup_cluster_lease(&nodes[leader].council, "failover", None)
        .await
        .unwrap();
    assert!(
        !nodes[leader]
            .council
            .desired_state()
            .await
            .test_leases
            .contains_key("failover")
    );
    root.cancel();
    resumed.await.unwrap();
}

/// An unreachable worker's duties can be resolved by the operator without
/// admitting its old identity when gossip or membership catches up later.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host"]
async fn decommissioned_worker_releases_cleanup_and_stays_retired_after_leader_change() {
    use reliaburger::cluster::retirement::DecommissionRequest;
    use reliaburger::meat::{Placement, Resources, SchedulingDecision};
    use reliaburger::testkit::lease::{
        LeaseError, TestLease, cleanup_cluster_lease, now_unix_millis,
    };
    assert!(cluster_tests_enabled());
    let operator = reliaburger::sesame::token::create_token(
        "operator",
        reliaburger::sesame::types::ApiRole::Admin,
        Default::default(),
        None,
    )
    .unwrap();
    let root = CancellationToken::new();
    let mut nodes =
        vec![start_node_for_test(300, vec![], &root, false, Some(operator.token.clone())).await];
    for index in 301..303 {
        nodes.push(
            start_node_for_test(
                index,
                vec![local(BASE_PORT + 3000)],
                &root,
                false,
                Some(operator.token.clone()),
            )
            .await,
        );
    }
    wait_until(
        "three settled voters",
        Duration::from_secs(60),
        async || {
            settled_voters(&nodes.iter().collect::<Vec<_>>())
                .is_some_and(|voters| voters.len() == 3)
        },
    )
    .await;
    let now = now_unix_millis();
    let lease = TestLease::new(
        "decommission".into(),
        "operator".into(),
        "operator".into(),
        "rbtest-decommission".into(),
        now,
        now + 600_000,
    )
    .unwrap();
    let app_id = AppId::new("cleanup", &lease.namespace);
    let mut spec = ported_service_spec(8182);
    spec.namespace = Some(lease.namespace.clone());
    for request in [
        RaftRequest::TestLeaseCreate(lease.clone()),
        RaftRequest::TestLeaseAppSpec {
            lease_id: lease.lease_id.clone(),
            observed_at_unix_ms: now,
            app_id: app_id.clone(),
            spec: Box::new(spec),
        },
        RaftRequest::SchedulingDecision(SchedulingDecision {
            app_id,
            placements: vec![Placement {
                node_id: NodeId::new(&nodes[2].name),
                resources: Resources::new(500, 1024 * 1024, 0),
            }],
        }),
    ] {
        assert!(!matches!(
            nodes[0].council.write(request).await.unwrap(),
            reliaburger::council::CouncilResponse::Refused { .. }
        ));
    }
    wait_until("worker workload", Duration::from_secs(30), async || {
        nodes[2]
            .resolve("cleanup")
            .await
            .is_some_and(|view| view.total_backends == 1)
    })
    .await;
    // This is the operator's external shutdown, before the attestation.
    nodes[2].shutdown.cancel();
    nodes[2].council.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), nodes[2].cmd_tx.closed())
        .await
        .unwrap();
    assert!(matches!(
        cleanup_cluster_lease(&nodes[0].council, &lease.lease_id, None).await,
        Err(LeaseError::CleanupPending)
    ));
    let request = DecommissionRequest {
        node_id: nodes[2].name.clone(),
        workloads_stopped: true,
        reason: "stopped for maintenance".into(),
    };
    // Use the real client and a follower to cover credential-preserving forwarding.
    let client = reliaburger::relish::client::BunClient::new_with_token(
        &format!("http://127.0.0.1:{}", nodes[1].api_port),
        Some(&operator.plaintext),
    );
    let record = client.decommission_node(&request).await.unwrap();
    assert_eq!(record.released_placements.get(&lease.lease_id), Some(&1));
    assert_eq!(client.decommission_node(&request).await.unwrap(), record);
    cleanup_cluster_lease(&nodes[0].council, &lease.lease_id, None)
        .await
        .unwrap();
    wait_until(
        "retired voter removed on both survivors",
        Duration::from_secs(30),
        async || {
            settled_voters(&[&nodes[0], &nodes[1]])
                .is_some_and(|voters| !voters.contains(&nodes[2].raft_id))
        },
    )
    .await;
    assert!(
        nodes[0]
            .council
            .add_learner(
                nodes[2].raft_id,
                reliaburger::council::CouncilNodeInfo::new(
                    local(BASE_PORT + 3020 + 1),
                    nodes[2].name.clone()
                )
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("retired")
    );
    nodes.push(
        start_node_for_test(
            303,
            vec![local(BASE_PORT + 3000)],
            &root,
            false,
            Some(operator.token.clone()),
        )
        .await,
    );
    // Every survivor, not just the leader, must have applied the promotion
    // before the leader dies, or the election below can never be won.
    wait_until(
        "fresh replacement voter applied by every member",
        Duration::from_secs(60),
        async || {
            settled_voters(&[&nodes[0], &nodes[1], &nodes[3]]).is_some_and(|voters| {
                voters == BTreeSet::from([nodes[0].raft_id, nodes[1].raft_id, nodes[3].raft_id])
            })
        },
    )
    .await;
    nodes[0].shutdown.cancel();
    nodes[0].council.shutdown().await.unwrap();
    let mut leader = None;
    wait_until(
        "successor after decommission",
        Duration::from_secs(60),
        async || {
            for index in [1, 3] {
                if nodes[index].is_leader().await {
                    leader = Some(index);
                    return true;
                }
            }
            false
        },
    )
    .await;
    let state = nodes[leader.unwrap()].council.desired_state().await;
    assert_eq!(
        state.security_state.crl.retired_nodes[&nodes[2].name],
        record
    );
    assert!(!state.test_leases.contains_key(&lease.lease_id));
    root.cancel();
}
