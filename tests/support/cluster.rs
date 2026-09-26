//! Shared start-up and wait helpers for the multi-node cluster tests.
//!
//! Three families live here, each copied across several test binaries
//! before this module existed:
//!
//! - small helpers: [`local`], [`cluster_tests_enabled`] and the polling
//!   [`wait_until`];
//! - an in-memory council fixture (five `CouncilNode`s on an
//!   `InMemoryRaftRouter`, three voters and two spares) and the leader waits
//!   that go with it;
//! - [`start_wired_node`], the same subsystems `bun --cluster` starts
//!   (gossip, Raft, reporting, a real `BunAgent` on `ProcessGrill`, the HTTP
//!   API, the leader scheduler and the placement reconciler), on one host
//!   with per-node port blocks.
//!
//! Included with `#[path = "support/cluster.rs"] mod cluster_support;` (or
//! `"../support/cluster.rs"` from `tests/suite/main.rs`).
//!
//! Every test binary compiles its own copy of this module and uses only part
//! of it, so the module allows dead code rather than making each consumer
//! silence the helpers it doesn't need.
#![allow(dead_code)]

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{RwLock, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use reliaburger::bun::agent::{AgentCommand, BunAgent, PartitionBlocklists};
use reliaburger::bun::api::{self, NodeMembershipInfo};
use reliaburger::cluster::identity::raft_id_from_name;
use reliaburger::cluster::orchestrate::{spawn_leader_scheduler, spawn_placement_reconciler};
use reliaburger::cluster::runtime::{self, ClusterParams, CouncilReconcilerConfig};
use reliaburger::config::node::{ReconstructionSection, ReportingTreeSection};
use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::selection::CouncilSelectionConfig;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo};
use reliaburger::grill::port::PortAllocator;
use reliaburger::grill::process::ProcessGrill;
use reliaburger::mayo::rollup_store::RollupStore;
use reliaburger::meat::NodeId;
use reliaburger::mustard::directory::NodeDirectory;
use reliaburger::mustard::membership::MembershipSnapshot;
use reliaburger::mustard::state::NodeState;
use reliaburger::reporting::aggregator::AggregatedState;
use reliaburger::sesame::auth::TokenStore;
use reliaburger::sesame::types::ApiToken;

#[path = "task_harness.rs"]
pub(crate) mod task_harness;
use task_harness::TestTasks;

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// A loopback socket address on `port`.
pub fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Whether the heavy cluster suite is enabled.
pub fn cluster_tests_enabled() -> bool {
    std::env::var("RELIABURGER_CLUSTER_TESTS").is_ok()
}

/// Poll `cond` every `poll` until it returns true or `timeout` elapses.
///
/// Checks once more after the deadline, so a condition that became true
/// during the last sleep still counts. Each binary passes its own interval:
/// the in-memory council tests poll every 50 ms to catch sub-second
/// hysteresis windows, the wired-node suites poll more gently.
pub async fn wait_until(timeout: Duration, poll: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(poll).await;
    }
}

/// The first leader any of `nodes` knows about, polling every 50 ms until
/// `timeout` elapses.
pub async fn wait_for_leader<N: Borrow<CouncilNode>>(
    nodes: &[N],
    timeout: Duration,
) -> Option<u64> {
    let start = tokio::time::Instant::now();
    loop {
        for node in nodes {
            if let Some(leader) = node.borrow().current_leader().await {
                return Some(leader);
            }
        }
        if start.elapsed() > timeout {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// In-memory council fixture
// ---------------------------------------------------------------------------

/// Names of the five in-memory council nodes.
pub const NAMES: [&str; 5] = ["node-1", "node-2", "node-3", "node-4", "node-5"];

/// Raft id of in-memory node `index`.
pub fn rid(index: usize) -> u64 {
    raft_id_from_name(NAMES[index])
}

/// Gossip address of in-memory node `index`.
pub fn gossip_addr(index: usize) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9443 + 2 * index as u16))
}

/// Council membership info of in-memory node `index`.
pub fn node_info(index: usize) -> CouncilNodeInfo {
    // Raft address = gossip address + a port offset of 1.
    CouncilNodeInfo::new(
        SocketAddr::from(([127, 0, 0, 1], 9444 + 2 * index as u16)),
        NAMES[index].to_string(),
    )
}

/// A gossip view of node `index`: alive, warm (backdated `first_seen` so the
/// candidate alive window is already satisfied at test start).
pub fn member(index: usize, now: Instant) -> MembershipSnapshot {
    MembershipSnapshot {
        node_id: NodeId::new(NAMES[index]),
        address: gossip_addr(index),
        state: NodeState::Alive,
        incarnation: 1,
        is_council: false,
        is_leader: false,
        labels: BTreeMap::new(),
        // node-4 older than node-5 so replacement selection is deterministic.
        first_seen: now - Duration::from_secs(700 - index as u64 * 10),
        resources: None,
    }
}

/// Raft timings fast enough for elections to settle in well under a second.
pub fn fast_council_config() -> CouncilConfig {
    CouncilConfig {
        heartbeat_interval_ms: 50,
        election_timeout_min_ms: 200,
        election_timeout_max_ms: 400,
        snapshot_threshold: 1000,
        max_in_snapshot_log_to_keep: 500,
    }
}

/// Sub-second hysteresis so the acceptance tests finish in seconds.
pub fn heal_reconciler_config() -> CouncilReconcilerConfig {
    CouncilReconcilerConfig {
        selection: CouncilSelectionConfig {
            min_node_age: Duration::from_secs(0),
            min_council_size: 3,
            max_council_size: 3,
            dead_window: Duration::from_millis(600),
            candidate_alive_window: Duration::from_millis(200),
            max_promotion_lag: 16,
            ..CouncilSelectionConfig::default()
        },
        tick_interval: Duration::from_millis(100),
        op_timeout: Duration::from_secs(1),
    }
}

/// Build a fresh five-node router with the first three initialised as voters.
pub async fn build_council() -> (Vec<Arc<CouncilNode>>, InMemoryRaftRouter) {
    let router = InMemoryRaftRouter::new();
    let mut nodes = Vec::new();
    for index in 0..NAMES.len() {
        let network = InMemoryRaftNetworkFactory::new(rid(index), router.clone());
        let node = CouncilNode::new(
            rid(index),
            fast_council_config(),
            network,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap();
        router.register(rid(index), node.raft().clone()).await;
        nodes.push(Arc::new(node));
    }

    let mut members = BTreeMap::new();
    for index in 0..3 {
        members.insert(rid(index), node_info(index));
    }
    nodes[0].initialize(members).await.unwrap();
    (nodes, router)
}

/// Index of the leader among the three initial voters, as any of them sees
/// it, or `None` if none reports one within five seconds.
pub async fn initial_voter_leader_index(nodes: &[Arc<CouncilNode>]) -> Option<usize> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        for node in &nodes[..3] {
            if let Some(leader) = node.current_leader().await
                && let Some(pos) = (0..3).find(|i| rid(*i) == leader)
            {
                return Some(pos);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Fully wired node
// ---------------------------------------------------------------------------

/// Where a wired node's API learns its peers' API addresses.
#[derive(Debug, Clone, Copy)]
pub enum MembershipSource {
    /// Alive gossip members, API port = gossip port + 3.
    Gossip,
    /// The gossip directory's advertised API endpoints.
    Directory,
}

/// How to start one fully wired node.
pub struct WiredNodeOptions {
    pub name: String,
    /// First port of the node's block: gossip, +1 Raft, +2 reporting, +3 API.
    pub gossip_port: u16,
    pub seeds: Vec<SocketAddr>,
    /// Cancelling this token stops the node.
    pub shutdown: CancellationToken,
    /// Prefix of the node's temp data directory, unique per test binary.
    pub data_dir_prefix: &'static str,
    pub stale_report_timeout_secs: u64,
    /// `Some(interval)` gives the node a Mayo store with rollups at that
    /// interval, and runs the autoscaler alongside the scheduler.
    pub metrics_rollup: Option<Duration>,
    /// `Some` runs the leader scheduler with this learning period.
    pub scheduler: Option<ReconstructionSection>,
    /// Run the leader's lease reaper, as Bun does.
    pub lease_reaper: bool,
    pub membership: MembershipSource,
    /// Internal service identity for the placement reconciler and the API.
    pub service_identity: Option<String>,
    /// Seeds the API token store; `None` leaves the API without one.
    pub operator_token: Option<ApiToken>,
    /// Serve the development test policy that admits fault injection.
    pub fault_injection: bool,
}

/// A running wired node and everything a test observes it through.
pub struct WiredNode {
    pub name: String,
    pub raft_id: u64,
    pub api_port: u16,
    pub council: Arc<CouncilNode>,
    pub metrics_rx: watch::Receiver<openraft::RaftMetrics<u64, CouncilNodeInfo>>,
    pub membership_rx: watch::Receiver<Vec<MembershipSnapshot>>,
    pub aggregated_rx: watch::Receiver<AggregatedState>,
    pub directory_rx: watch::Receiver<NodeDirectory>,
    pub partition_blocklists: PartitionBlocklists,
    pub rollup_store: Arc<RwLock<RollupStore>>,
    /// This node's local metrics store, when `metrics_rollup` is set. The
    /// rollup worker ships its per-minute aggregates to the leader.
    pub mayo: Option<Arc<RwLock<reliaburger::mayo::store::MayoStore>>>,
    /// This node's agent command channel.
    pub cmd_tx: mpsc::Sender<AgentCommand>,
    pub membership_table: Arc<RwLock<Vec<NodeMembershipInfo>>>,
    pub token_store: Option<TokenStore>,
    /// Whether this node's Raft metrics name itself as leader.
    pub thinks_leader: watch::Receiver<bool>,
    pub reconciler: JoinHandle<()>,
    /// Cancelling this token kills this node.
    pub shutdown: CancellationToken,
    pub _runtime: runtime::ClusterRuntime,
    pub _tasks: TestTasks,
}

/// Start one fully wired node: the same subsystems `bun --cluster` runs.
pub async fn start_wired_node(options: WiredNodeOptions) -> WiredNode {
    let WiredNodeOptions {
        name,
        gossip_port,
        seeds,
        shutdown,
        data_dir_prefix,
        stale_report_timeout_secs,
        metrics_rollup,
        scheduler,
        lease_reaper,
        membership,
        service_identity,
        operator_token,
        fault_injection,
    } = options;
    let raft_port = gossip_port + 1;
    let reporting_port = gossip_port + 2;
    let api_port = gossip_port + 3;

    let data_dir = std::env::temp_dir().join(format!("{data_dir_prefix}-{name}-{gossip_port}"));
    let _ = std::fs::remove_dir_all(&data_dir);
    let reconciler_state_dir = data_dir.clone();

    let mayo = metrics_rollup.map(|_| {
        Arc::new(RwLock::new(reliaburger::mayo::store::MayoStore::new(
            data_dir.join("metrics"),
        )))
    });
    let readiness = reliaburger::bun::readiness::ReadinessTracker::new();
    readiness.register("agent", true).await;

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
                stale_report_timeout_secs,
            },
            seeds,
            wrapping_ikm: None,
            bootstrap_security_state: None,
            data_dir,
            mayo: mayo.clone(),
            rollup_interval: metrics_rollup.unwrap_or(Duration::from_secs(60)),
            identity: None,
            backup: Default::default(),
            labels: BTreeMap::new(),
            self_disk_pressured_rx: None,
            readiness: Some(readiness.clone()),
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
    let partition_blocklists = handle.partition_blocklists.clone();
    let aggregated_rx = cluster_runtime.aggregated_rx.clone();
    let rollup_store = Arc::clone(&cluster_runtime.rollup_store);
    let directory_rx = cluster_runtime.directory_rx.clone();

    // Real agent with a ProcessGrill, built with the cluster handle so it
    // answers reporting snapshots with real capacity. `ClusterHandle` isn't
    // `Clone` (it owns `snapshot_rx`), so the agent takes it and the node
    // keeps clones of the watch receivers above.
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    let mut agent = BunAgent::with_cluster(
        ProcessGrill::new(),
        PortAllocator::new(gossip_port + 100, gossip_port + 400),
        cmd_rx,
        shutdown.clone(),
        handle,
        "default".to_string(),
    );
    agent.set_volumes_dir(reconciler_state_dir.join("volumes"));
    agent.set_node_capacity(8000, 16384);
    agent.set_readiness_tracker(readiness.clone());
    // Several agents share this host; don't spawn nft against the real
    // host firewall (`with_cluster` enables it by default on Linux).
    agent.set_perimeter_enabled(false);
    let agent_task = reliaburger::bun::readiness::spawn_owned(
        "agent",
        true,
        readiness.clone(),
        shutdown.clone(),
        move |ready| async move { agent.run_with_readiness(ready).await },
    );
    let mut tasks = vec![agent_task];

    // DELETE starts cleanup; the leader's reaper finishes it once workers
    // acknowledge retirement. Match Bun's lifecycle rather than leaving the
    // fixture permanently at HTTP 202 after the final acknowledgement.
    if lease_reaper {
        tasks.push(reliaburger::testkit::lease::spawn_cluster_lease_reaper(
            Arc::clone(&council),
            shutdown.clone(),
        ));
    }

    let membership_table: Arc<RwLock<Vec<NodeMembershipInfo>>> = Arc::new(RwLock::new(Vec::new()));
    tasks.push(match membership {
        MembershipSource::Gossip => spawn_gossip_membership_table(
            membership_rx.clone(),
            Arc::clone(&membership_table),
            shutdown.clone(),
        ),
        MembershipSource::Directory => spawn_directory_membership_table(
            directory_rx.clone(),
            Arc::clone(&membership_table),
            shutdown.clone(),
        ),
    });

    // Leader scheduler with a fast learning period, so a fresh leader starts
    // scheduling within seconds of gaining coverage.
    let capacity_admission = scheduler.map(|reconstruction| {
        spawn_leader_scheduler(
            Arc::clone(&council),
            membership_rx.clone(),
            aggregated_rx.clone(),
            false,
            reconstruction,
            None,
            shutdown.clone(),
        )
    });
    if capacity_admission.is_some() && metrics_rollup.is_some() {
        reliaburger::cluster::orchestrate::spawn_autoscaler(
            Arc::clone(&council),
            Arc::clone(&rollup_store),
            Duration::from_millis(500),
            shutdown.clone(),
        );
    }

    // Placement reconciler: resolves the leader through Raft metrics OR the
    // gossip directory; on worker nodes only the latter exists.
    let reconciler = spawn_placement_reconciler(
        name.clone(),
        metrics_rx.clone(),
        directory_rx.clone(),
        2, // api = raft + 2 in this port block
        service_identity.clone(),
        cmd_tx.clone(),
        shutdown.clone(),
        reliaburger::cluster::ClusterHttp::plaintext(),
        Some(reconciler_state_dir),
        reliaburger::config::node::RuntimeSection::default().stop_confirmation_timeout(),
    );

    // HTTP API (serves /v1/placements for the reconcilers).
    let listener = tokio::net::TcpListener::bind(local(api_port))
        .await
        .unwrap();
    let token_store = operator_token.map(|token| Arc::new(RwLock::new(vec![token])));
    let app = if fault_injection {
        let static_capabilities = reliaburger::bun::capabilities::StaticCapabilities {
            cluster_mode: true,
            node_id: name.clone(),
            test_policy: reliaburger::testkit::safety::ClusterTestPolicy {
                safety_class: reliaburger::testkit::safety::ClusterSafetyClass::Development,
                allowed_operations: std::collections::BTreeSet::from([
                    reliaburger::testkit::safety::OperationPermission::AlterNodeState,
                    reliaburger::testkit::safety::OperationPermission::InjectWorkloadFaults,
                    reliaburger::testkit::safety::OperationPermission::ProvisionIsolatedWorkloads,
                    reliaburger::testkit::safety::OperationPermission::SaturateCapacity,
                ]),
                ..reliaburger::testkit::safety::ClusterTestPolicy::default()
            },
            ..reliaburger::bun::capabilities::StaticCapabilities::default()
        };
        api::router_with_upgrade(
            cmd_tx.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(Arc::clone(&council)),
            token_store.clone(),
            service_identity,
            None,
            Some(Arc::clone(&membership_table)),
            None,
            None,
            api_port,
            None,
            None,
            None,
            "default".to_string(),
            Some(name.clone()),
            900,
            reliaburger::cluster::ClusterHttp::plaintext(),
            5050,
            "http",
            256 * 1024 * 1024,
            false,
            static_capabilities,
            readiness,
            None,
            None,
        )
    } else {
        api::router(
            cmd_tx.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(Arc::clone(&council)),
            token_store.clone(),
            service_identity,
            None,
            Some(Arc::clone(&membership_table)),
            None,
            api_port,
            None,
        )
    };
    let app = match capacity_admission {
        Some(admission) => app.layer(axum::Extension(admission)),
        None => app,
    };
    let api_shutdown = shutdown.clone();
    tasks.push(tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { api_shutdown.cancelled().await })
            .await
            .ok();
    }));

    // Derive a leadership watch from the Raft metrics.
    let (leader_tx, thinks_leader) = watch::channel(false);
    let mut leader_metrics_rx = metrics_rx.clone();
    tasks.push(tokio::spawn(async move {
        loop {
            let is_leader = {
                let m = leader_metrics_rx.borrow();
                m.current_leader == Some(m.id)
            };
            let _ = leader_tx.send(is_leader);
            if leader_metrics_rx.changed().await.is_err() {
                break;
            }
        }
    }));

    WiredNode {
        raft_id: raft_id_from_name(&name),
        name,
        api_port,
        council,
        metrics_rx,
        membership_rx,
        aggregated_rx,
        directory_rx,
        partition_blocklists,
        rollup_store,
        mayo,
        cmd_tx,
        membership_table,
        token_store,
        thinks_leader,
        reconciler,
        shutdown: shutdown.clone(),
        _runtime: cluster_runtime,
        _tasks: TestTasks::new(shutdown, tasks),
    }
}

/// Keep `table` in step with the alive gossip members (API = gossip + 3).
fn spawn_gossip_membership_table(
    mut rx: watch::Receiver<Vec<MembershipSnapshot>>,
    table: Arc<RwLock<Vec<NodeMembershipInfo>>>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let snapshot: Vec<NodeMembershipInfo> = rx
                .borrow()
                .iter()
                .filter(|m| m.state == NodeState::Alive)
                .map(|m| NodeMembershipInfo {
                    node_id: m.node_id.clone(),
                    address: SocketAddr::new(m.address.ip(), m.address.port() + 3),
                    api_advertised: true,
                })
                .collect();
            *table.write().await = snapshot;
            tokio::select! {
                _ = shutdown.cancelled() => break,
                changed = rx.changed() => if changed.is_err() { break },
            }
        }
    })
}

/// Keep `table` in step with the API endpoints the gossip directory carries.
fn spawn_directory_membership_table(
    mut rx: watch::Receiver<NodeDirectory>,
    table: Arc<RwLock<Vec<NodeMembershipInfo>>>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let snapshot = rx
                .borrow()
                .endpoints
                .iter()
                .map(|(node_id, endpoints)| NodeMembershipInfo {
                    node_id: node_id.clone(),
                    address: endpoints.api_address,
                    api_advertised: true,
                })
                .collect();
            *table.write().await = snapshot;
            tokio::select! {
                _ = shutdown.cancelled() => break,
                result = rx.changed() => if result.is_err() { break; },
            }
        }
    })
}
