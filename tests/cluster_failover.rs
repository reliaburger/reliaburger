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

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{RwLock, mpsc, watch};
use tokio_util::sync::CancellationToken;

use reliaburger::bun::agent::BunAgent;
use reliaburger::bun::api;
use reliaburger::cluster::orchestrate::{spawn_leader_scheduler, spawn_placement_reconciler};
use reliaburger::cluster::runtime::{self, ClusterParams};
use reliaburger::config::node::ReportingTreeSection;
use reliaburger::council::types::RaftRequest;
use reliaburger::grill::port::PortAllocator;
use reliaburger::grill::process::ProcessGrill;
use reliaburger::meat::{AppId, NodeId};
use reliaburger::reporting::aggregator::AggregatedState;

/// Whether the heavy cluster suite is enabled.
fn cluster_tests_enabled() -> bool {
    std::env::var("RELIABURGER_CLUSTER_TESTS").is_ok()
}

const NODE_COUNT: usize = 9;
const BASE_PORT: u16 = 19510;

fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Everything the test needs to observe (and kill) one node.
struct NodeHarness {
    name: String,
    raft_id: u64,
    council: Arc<reliaburger::council::CouncilNode>,
    metrics_rx:
        watch::Receiver<openraft::RaftMetrics<u64, reliaburger::council::types::CouncilNodeInfo>>,
    aggregated_rx: watch::Receiver<AggregatedState>,
    /// Cancelling this token kills THIS node only.
    shutdown: CancellationToken,
}

impl NodeHarness {
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

    fn is_voter(&self) -> bool {
        let metrics = self.metrics_rx.borrow();
        metrics
            .membership_config
            .membership()
            .voter_ids()
            .any(|id| id == self.raft_id)
    }
}

/// Start one fully wired node: the same subsystems `bun --cluster` runs.
async fn start_node(index: usize, seeds: Vec<SocketAddr>, root: &CancellationToken) -> NodeHarness {
    let name = format!("fo{index}");
    let gossip_port = BASE_PORT + (index as u16) * 10;
    let raft_port = gossip_port + 1;
    let reporting_port = gossip_port + 2;
    let api_port = gossip_port + 3;
    let shutdown = root.child_token();

    let data_dir = std::env::temp_dir().join(format!("rb-failover-{name}-{gossip_port}"));
    let _ = std::fs::remove_dir_all(&data_dir);
    let reconciler_state_dir = data_dir.clone();

    let (handle, cluster_runtime) = runtime::start(
        ClusterParams {
            node_name: name.clone(),
            gossip_addr: local(gossip_port),
            raft_port,
            reporting_port,
            api_port,
            reporting_config: ReportingTreeSection {
                report_interval_secs: 1,
                max_events_per_report: 100,
                stale_report_timeout_secs: 10,
            },
            seeds,
            wrapping_ikm: None,
            bootstrap_security_state: None,
            data_dir,
            mayo: None,
            rollup_interval: Duration::from_secs(60),
            identity: None,
            backup: Default::default(),
            labels: std::collections::BTreeMap::new(),
        },
        shutdown.clone(),
    )
    .await
    .unwrap();

    let council = handle.council.clone().expect("cluster mode has a council");
    let membership_rx = handle.membership_rx.clone();
    let metrics_rx = handle
        .raft_metrics_rx
        .clone()
        .expect("cluster mode has raft metrics");
    let aggregated_rx = cluster_runtime.aggregated_rx.clone();
    let directory_rx = cluster_runtime.directory_rx.clone();
    Box::leak(Box::new(cluster_runtime));

    // Real agent answering reporting snapshots with real capacity.
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    let mut agent = BunAgent::with_cluster(
        ProcessGrill::new(),
        PortAllocator::new(gossip_port + 100, gossip_port + 400),
        cmd_rx,
        shutdown.clone(),
        handle,
    );
    agent.set_node_capacity(8000, 16384);
    // Co-located test agents must not touch the shared host firewall.
    agent.set_perimeter_enabled(false);
    tokio::spawn(async move { agent.run().await });

    // Leader scheduler with a fast learning period, so a fresh leader
    // starts scheduling within seconds of gaining coverage.
    spawn_leader_scheduler(
        Arc::clone(&council),
        membership_rx.clone(),
        aggregated_rx.clone(),
        reliaburger::config::node::ReconstructionSection {
            report_threshold_percent: 80,
            learning_period_timeout_secs: 5,
            large_cluster_timeout_secs: 10,
            large_cluster_node_count: 5000,
        },
        shutdown.clone(),
    );

    // Placement reconciler: resolves the leader through Raft metrics OR the
    // gossip directory — on the two worker nodes only the latter exists.
    spawn_placement_reconciler(
        name.clone(),
        metrics_rx.clone(),
        directory_rx,
        2, // api = raft + 2 in this port block
        None,
        cmd_tx.clone(),
        shutdown.clone(),
        reliaburger::cluster::ClusterHttp::plaintext(),
        Some(reconciler_state_dir),
    );

    // HTTP API (serves /v1/placements for the reconcilers).
    let listener = tokio::net::TcpListener::bind(local(api_port))
        .await
        .unwrap();
    let router = api::router(
        cmd_tx,
        None,
        None,
        None,
        None,
        None,
        Some(Arc::clone(&council)),
        None,
        None,
        None,
        Some(Arc::new(RwLock::new(Vec::new()))),
        None,
        api_port,
        None,
    );
    let api_shutdown = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move { api_shutdown.cancelled().await })
            .await
            .ok();
    });

    NodeHarness {
        raft_id: reliaburger::cluster::identity::raft_id_from_name(&name),
        name,
        council,
        metrics_rx,
        aggregated_rx,
        shutdown,
    }
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
        cpu = "100m-200m"
        "#
    ))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn eight_plus_node_cluster_reconciles_and_reports_through_leader_failover() {
    if !cluster_tests_enabled() {
        eprintln!("skipping cluster integration test (set RELIABURGER_CLUSTER_TESTS=1)");
        return;
    }

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

    let workers: Vec<&NodeHarness> = nodes.iter().filter(|n| !n.is_voter()).collect();
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

    let survivors: Vec<&NodeHarness> = nodes.iter().filter(|n| n.name != old_leader_name).collect();

    let mut new_leader: Option<&NodeHarness> = None;
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
