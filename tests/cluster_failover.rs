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
//! Every test is hermetic (#721): a [`Fixture`] gives it a fresh data
//! directory and leased port blocks, and the tests ask the cluster which
//! node leads instead of assuming the bootstrap node still does.
//!
//! Gated behind `RELIABURGER_CLUSTER_TESTS=1` (the
//! `RELIABURGER_UPGRADE_TESTS` precedent): nine nodes with seven-voter Raft
//! elections are far past a 2-core CI budget. Run via
//! `RELIABURGER_CLUSTER_TESTS=1 cargo test --test cluster_failover`.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use reliaburger::cluster::orchestrate::spawn_placement_reconciler;
use reliaburger::council::types::RaftRequest;
use reliaburger::council::{CouncilError, CouncilResponse};
use reliaburger::meat::{AppId, NodeId};
use reliaburger::reporting::aggregator::AggregatedState;

#[path = "support/bun_process.rs"]
mod bun_process;
#[path = "support/cluster.rs"]
mod cluster_support;
use bun_process::{PortBlockReservation, reserve_port_block_lease};
use cluster_support::{
    MembershipSource, WiredNode, WiredNodeOptions, clear_fixture_dir, cluster_tests_enabled, local,
    start_wired_node,
};

const RETIREMENT_SERVICE_TOKEN: &str = "cluster-test-retirement-service";
const NODE_COUNT: usize = 9;
/// Gossip, Raft, reporting and API: `start_wired_node`'s port block.
const PORTS_PER_NODE: u16 = 4;

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

    /// The ingress routes this node's Wrapper would serve.
    async fn routes(&self) -> Vec<reliaburger::wrapper::types::RouteInfo> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .cmd_tx
            .send(reliaburger::bun::agent::AgentCommand::Routes { response: tx })
            .await
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
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

/// One test's cluster resources, shared with nothing else.
///
/// Each node keeps its data under the fixture's own temporary directory and
/// binds a port block leased from the range the other suites lease from, so
/// a parallel suite can't hold one of its ports and an earlier run can't
/// leave state where this one looks (#721).
struct Fixture {
    /// Cancelling this kills every node; each node also has a child token.
    root: CancellationToken,
    data: tempfile::TempDir,
    /// Leased port blocks by node index, held until the test ends so a
    /// killed node's ports stay ours.
    ports: BTreeMap<usize, PortBlockReservation>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: CancellationToken::new(),
            data: tempfile::Builder::new()
                .prefix("rb-failover-")
                .tempdir()
                .unwrap(),
            ports: BTreeMap::new(),
        }
    }

    /// The first (gossip) port of node `index`'s block, leased on first use.
    fn gossip_port(&mut self, index: usize) -> u16 {
        self.ports
            .entry(index)
            .or_insert_with(|| reserve_port_block_lease(PORTS_PER_NODE))
            .base
    }

    fn gossip(&mut self, index: usize) -> SocketAddr {
        local(self.gossip_port(index))
    }

    /// Start one fully wired node: the same subsystems `bun --cluster` runs.
    async fn start_node(
        &mut self,
        index: usize,
        seeds: Vec<SocketAddr>,
        schedule: bool,
        operator: Option<reliaburger::sesame::types::ApiToken>,
    ) -> WiredNode {
        self.start_node_with(index, seeds, schedule, operator, NodePlacement::default())
            .await
    }

    /// Start a node that schedules, labelled `rack=<rack>`, whose API
    /// leaves a dead peer out of cluster-wide reads once gossip says so.
    async fn start_racked_node(
        &mut self,
        index: usize,
        seeds: Vec<SocketAddr>,
        operator: reliaburger::sesame::types::ApiToken,
        rack: &str,
    ) -> WiredNode {
        let placement = NodePlacement {
            labels: BTreeMap::from([("rack".to_string(), rack.to_string())]),
            membership: Some(MembershipSource::Gossip),
            test_operations: false,
        };
        self.start_node_with(index, seeds, true, Some(operator), placement)
            .await
    }

    async fn start_node_with(
        &mut self,
        index: usize,
        seeds: Vec<SocketAddr>,
        schedule: bool,
        operator: Option<reliaburger::sesame::types::ApiToken>,
        placement: NodePlacement,
    ) -> WiredNode {
        start_wired_node(WiredNodeOptions {
            name: format!("fo{index}"),
            gossip_port: self.gossip_port(index),
            seeds,
            // Cancelling a node's own token kills that node only.
            shutdown: self.root.child_token(),
            data_dir_prefix: "rb-failover",
            data_root: Some(self.data.path().to_path_buf()),
            stale_report_timeout_secs: 10,
            stale_node_grace: None,
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
            membership: placement.membership.unwrap_or(MembershipSource::Directory),
            service_identity: Some(RETIREMENT_SERVICE_TOKEN.into()),
            operator_token: operator,
            fault_injection: placement.test_operations,
            labels: placement.labels,
            keep_data_dir: false,
            task_arrays: None,
            task_arrays_on_agent_budget: None,
            process_only: false,
            council_size_cap: None,
        })
        .await
    }
}

/// Where a node sits: its labels, and where its API learns its peers.
#[derive(Default)]
struct NodePlacement {
    labels: BTreeMap<String, String>,
    /// The gossip directory when `None`.
    membership: Option<MembershipSource>,
    /// Serve the development test policy, which lets a client take test
    /// leases.
    test_operations: bool,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.root.cancel();
        // A node that ran as root may leave an identity tmpfs mounted, which
        // `TempDir` alone can't remove. Panicking here could abort a test
        // that is already unwinding, so a failure is only reported.
        if let Err(error) = clear_fixture_dir(self.data.path()) {
            eprintln!(
                "cannot clear fixture directory {}: {error}",
                self.data.path().display()
            );
        }
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

/// The nodes at `indexes`, for helpers that take a slice of references.
fn pick<'a>(nodes: &'a [WiredNode], indexes: &[usize]) -> Vec<&'a WiredNode> {
    indexes.iter().map(|&index| &nodes[index]).collect()
}

/// The index of whichever node in `among` leads right now, if one does.
///
/// Leadership can move at any moment under load (an election timeout on a
/// busy host is enough), so tests ask each time rather than assuming the
/// bootstrap node, or the last leader they saw, still leads.
async fn leader_of(nodes: &[WiredNode], among: &[usize]) -> Option<usize> {
    for &index in among {
        if nodes[index].is_leader().await {
            return Some(index);
        }
    }
    None
}

async fn wait_for_leader(nodes: &[WiredNode], among: &[usize], timeout: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(leader) = leader_of(nodes, among).await {
            return leader;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no leader among {:?} within {timeout:?}",
            pick(nodes, among)
                .iter()
                .map(|node| &node.name)
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The current leader's aggregated reports, if a node in `among` leads.
async fn leader_state(nodes: &[WiredNode], among: &[usize]) -> Option<AggregatedState> {
    let leader = leader_of(nodes, among).await?;
    Some(nodes[leader].aggregated_rx.borrow().clone())
}

/// Write `request` through whichever node in `among` leads, retrying while
/// leadership moves. A `SchedulingDecision` is wrapped in the leader's own
/// `SchedulingDecisions` precondition at each attempt.
async fn write_through_leader(
    nodes: &[WiredNode],
    among: &[usize],
    request: RaftRequest,
) -> CouncilResponse {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let leader = wait_for_leader(nodes, among, Duration::from_secs(30)).await;
        let council = &nodes[leader].council;
        let attempt = match &request {
            RaftRequest::SchedulingDecision(decision) => RaftRequest::SchedulingDecisions {
                expected_log_id: council.desired_state().await.last_applied_log,
                decisions: vec![decision.clone()],
            },
            request => request.clone(),
        };
        match council.write(attempt).await {
            Ok(response) => return response,
            // Not appended anywhere, so the retry can't apply it twice.
            Err(error @ CouncilError::ForwardToLeader { .. }) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "leadership kept moving: {error}"
                );
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(error) => panic!("write through {} failed: {error}", nodes[leader].name),
        }
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
        script = "exec sleep {command_seconds}"
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
        script = "exec sleep 600"
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

/// Kill `node` the way `kill -9` would: its spawned tasks and its
/// in-process Raft core both stop.
async fn kill(node: &WiredNode) {
    node.shutdown.cancel();
    // The cancellation token stops the node's spawned tasks (RPC server,
    // gossip, reporting), but the in-process Raft core only dies with an
    // explicit shutdown.
    node.council.shutdown().await.ok();
}

/// A three-voter cluster's roles once its membership settles: the leader and
/// the two other nodes, so a test never assumes which node won.
async fn settled_roles(nodes: &[WiredNode]) -> (usize, [usize; 2]) {
    wait_until(
        "three settled voters",
        Duration::from_secs(60),
        async || settled_voters(&pick(nodes, &[0, 1, 2])).is_some_and(|voters| voters.len() == 3),
    )
    .await;
    let leader = wait_for_leader(nodes, &[0, 1, 2], Duration::from_secs(30)).await;
    let mut others = (0..3).filter(|&index| index != leader);
    let (Some(first), Some(second)) = (others.next(), others.next()) else {
        unreachable!("three nodes have two that don't lead");
    };
    (leader, [first, second])
}

/// 12b.4: a service deployed on node B resolves + routes from node A. Every
/// node overlays the leader's replicated endpoint catalogue onto its local
/// service map, so cross-node resolution works even though the backend runs
/// elsewhere. Also proves the catalogue survives a leader change.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn service_on_one_node_resolves_and_survives_leader_change_from_another() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );

    let mut fixture = Fixture::new();
    // Three nodes, all voters (cap is seven): killing the leader leaves a
    // two-node quorum, so a survivor takes over cleanly. Three keeps the
    // election light while still proving cross-node resolve + failover.
    const N: usize = 3;
    let all: Vec<usize> = (0..N).collect();
    let mut nodes = Vec::with_capacity(N);
    nodes.push(fixture.start_node(100, vec![], true, None).await);
    for index in 101..(100 + N) {
        let seed = fixture.gossip(100);
        nodes.push(fixture.start_node(index, vec![seed], true, None).await);
    }

    // Wait for the council to commit all three as voters before deploying:
    // killing the leader before the voter set settles can leave the survivors
    // without a committed quorum, and election stalls.
    wait_until(
        "three settled voters",
        Duration::from_secs(60),
        async || settled_voters(&pick(&nodes, &all)).is_some_and(|voters| voters.len() == N),
    )
    .await;
    wait_until(
        "all nodes reporting to the leader",
        Duration::from_secs(60),
        async || {
            leader_state(&nodes, &all)
                .await
                .is_some_and(|state| state.reports.len() == N)
        },
    )
    .await;

    // Deploy one replica of a ported service via the leader.
    write_through_leader(
        &nodes,
        &all,
        RaftRequest::AppSpec {
            app_id: AppId::new("redis", "default"),
            spec: Box::new(ported_service_spec(6379)),
        },
    )
    .await;

    // Wait until exactly one node reports it running.
    let mut host_node = None;
    wait_until(
        "redis running on some node",
        Duration::from_secs(90),
        async || {
            host_node = leader_state(&nodes, &all)
                .await
                .and_then(|state| node_running_app(&state, "redis"));
            host_node.is_some()
        },
    )
    .await;
    let host_node = host_node.unwrap();
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
    let old_leader = wait_for_leader(&nodes, &all, Duration::from_secs(30)).await;
    kill(&nodes[old_leader]).await;
    let survivors: Vec<usize> = all
        .iter()
        .copied()
        .filter(|&index| index != old_leader)
        .collect();
    wait_for_leader(&nodes, &survivors, Duration::from_secs(90)).await;

    // A surviving node still resolves redis from the post-failover catalogue,
    // whether or not the replica had to move.
    let resolver = &nodes[survivors[0]];
    wait_until(
        "redis still resolves after the leader change",
        Duration::from_secs(90),
        async || match resolver.resolve("redis").await {
            Some(info) => info.total_backends >= 1,
            None => false,
        },
    )
    .await;

    fixture.root.cancel();
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// A ported service with an ingress route on `host`, plus `extra` ingress
/// settings.
fn ingress_service_spec(port: u16, host: &str, extra: &str) -> reliaburger::config::app::AppSpec {
    toml::from_str(&format!(
        r#"
        script = "exec sleep 600"
        replicas = 1
        port = {port}
        [ingress]
        host = "{host}"
        {extra}
        "#
    ))
    .unwrap()
}

/// #700: one app with an ingress route no node can install (`tls =
/// "acme"`) made every node refuse every later catalogue, so discovery and
/// ingress froze cluster-wide. `relish apply` now refuses that spec; the
/// test writes it straight to the council to stand for one admitted before
/// the check. Every node must skip that one route and keep publishing.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn invalid_ingress_on_one_app_leaves_discovery_working_cluster_wide() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );

    let mut fixture = Fixture::new();
    const N: usize = 3;
    let all: Vec<usize> = (0..N).collect();
    let mut nodes = Vec::with_capacity(N);
    nodes.push(fixture.start_node(200, vec![], true, None).await);
    for index in 201..(200 + N) {
        let seed = fixture.gossip(200);
        nodes.push(fixture.start_node(index, vec![seed], true, None).await);
    }
    wait_until(
        "three settled voters",
        Duration::from_secs(60),
        async || settled_voters(&pick(&nodes, &all)).is_some_and(|voters| voters.len() == N),
    )
    .await;
    wait_until(
        "all nodes reporting to the leader",
        Duration::from_secs(60),
        async || {
            leader_state(&nodes, &all)
                .await
                .is_some_and(|state| state.reports.len() == N)
        },
    )
    .await;

    write_through_leader(
        &nodes,
        &all,
        RaftRequest::AppSpec {
            app_id: AppId::new("broken", "default"),
            spec: Box::new(ingress_service_spec(6380, "broken.local", "tls = \"acme\"")),
        },
    )
    .await;
    wait_until(
        "broken running on some node",
        Duration::from_secs(90),
        async || {
            leader_state(&nodes, &all)
                .await
                .and_then(|state| node_running_app(&state, "broken"))
                .is_some()
        },
    )
    .await;

    // Deployed after the bad route: it reaches every node only if the
    // catalogue keeps advancing past it.
    write_through_leader(
        &nodes,
        &all,
        RaftRequest::AppSpec {
            app_id: AppId::new("redis", "default"),
            spec: Box::new(ingress_service_spec(6379, "redis.local", "")),
        },
    )
    .await;
    wait_until(
        "redis resolves and routes on every node",
        Duration::from_secs(90),
        async || {
            for node in &nodes {
                let resolves = node
                    .resolve("redis")
                    .await
                    .is_some_and(|info| info.healthy_backends >= 1);
                let routes = node.routes().await;
                let routed = routes
                    .iter()
                    .any(|route| route.host == "redis.local" && route.healthy_backends >= 1);
                if !resolves || !routed {
                    return false;
                }
            }
            true
        },
    )
    .await;
    for node in &nodes {
        assert!(
            node.resolve("broken").await.is_some(),
            "{}: the bad app's own service must still resolve",
            node.name
        );
        assert!(
            !node
                .routes()
                .await
                .iter()
                .any(|route| route.host == "broken.local"),
            "{}: the invalid route must never be installed",
            node.name
        );
    }

    fixture.root.cancel();
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// A one-replica ported service at `version`: the version only changes
/// the environment, so writing a new one rolls the app.
///
/// It ignores SIGTERM, so stopping the old instance takes the whole
/// `drain_timeout` before the kill, the way a real server finishing its
/// requests does. That holds the rollout between "old instance stopping"
/// and "new instance finalised" for longer than a report interval.
fn versioned_service_spec(port: u16, version: &str) -> reliaburger::config::app::AppSpec {
    toml::from_str(&format!(
        r#"
        script = "trap '' TERM; while true; do sleep 0.2; done"
        replicas = 1
        port = {port}
        [env]
        VERSION = "{version}"
        [deploy]
        drain_timeout = "3s"
        "#
    ))
    .unwrap()
}

/// #1353: rolling a one-replica service with the default `max_surge = 1`,
/// `max_unavailable = 0` must keep a healthy backend in every node's view
/// the whole way through. The burger demo's `cheese` showed 3 to 6 s in
/// which no node had one, and callers got `EPERM` from the connect hook.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn one_replica_rollout_keeps_a_healthy_backend_on_every_node() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );

    let mut fixture = Fixture::new();
    const N: usize = 3;
    let all: Vec<usize> = (0..N).collect();
    let mut nodes = Vec::with_capacity(N);
    nodes.push(fixture.start_node(300, vec![], true, None).await);
    for index in 301..(300 + N) {
        let seed = fixture.gossip(300);
        nodes.push(fixture.start_node(index, vec![seed], true, None).await);
    }
    wait_until(
        "three settled voters",
        Duration::from_secs(60),
        async || settled_voters(&pick(&nodes, &all)).is_some_and(|voters| voters.len() == N),
    )
    .await;
    wait_until(
        "all nodes reporting to the leader",
        Duration::from_secs(60),
        async || {
            leader_state(&nodes, &all)
                .await
                .is_some_and(|state| state.reports.len() == N)
        },
    )
    .await;

    let app_id = AppId::new("cheese", "default");
    write_through_leader(
        &nodes,
        &all,
        RaftRequest::AppSpec {
            app_id: app_id.clone(),
            spec: Box::new(versioned_service_spec(6391, "v1")),
        },
    )
    .await;
    wait_until(
        "cheese v1 resolves with a healthy backend on every node",
        Duration::from_secs(90),
        async || {
            for node in &nodes {
                let healthy = node
                    .resolve("cheese")
                    .await
                    .is_some_and(|info| info.healthy_backends >= 1);
                if !healthy {
                    return false;
                }
            }
            true
        },
    )
    .await;
    // Local and remote views name a backend differently (instance id here,
    // `node:ip:port` elsewhere), but its host port is the same everywhere.
    let original: BTreeSet<u16> = nodes[0]
        .resolve("cheese")
        .await
        .expect("cheese resolves")
        .backends
        .into_iter()
        .map(|backend| backend.host_port)
        .collect();

    write_through_leader(
        &nodes,
        &all,
        RaftRequest::AppSpec {
            app_id,
            spec: Box::new(versioned_service_spec(6391, "v2")),
        },
    )
    .await;

    // Sample every node's installed view until each one names only the
    // replacement, healthy. Every sample must have a healthy backend.
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(120);
    let mut gaps: Vec<String> = Vec::new();
    loop {
        let mut rolled = 0;
        for node in &nodes {
            let view = node.resolve("cheese").await;
            let at = started.elapsed();
            if !view.as_ref().is_some_and(|info| info.healthy_backends >= 1) {
                gaps.push(format!("{} at {at:?}: {view:?}", node.name));
            }
            let replaced = view.is_some_and(|info| {
                !info.backends.is_empty()
                    && info
                        .backends
                        .iter()
                        .all(|backend| backend.healthy && !original.contains(&backend.host_port))
            });
            if replaced {
                rolled += 1;
            }
        }
        if rolled == N {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the rollout of cheese v2 never reached every node's view; gaps: {gaps:#?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        gaps.is_empty(),
        "a one-replica rollout left nodes without a healthy cheese backend \
         ({} samples):\n{}",
        gaps.len(),
        gaps.join("\n")
    );

    fixture.root.cancel();
    tokio::time::sleep(Duration::from_millis(500)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn eight_plus_node_cluster_reconciles_and_reports_through_leader_failover() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );

    let mut fixture = Fixture::new();
    let all: Vec<usize> = (0..NODE_COUNT).collect();
    let mut nodes = Vec::with_capacity(NODE_COUNT);
    nodes.push(fixture.start_node(0, vec![], true, None).await);
    for index in 1..NODE_COUNT {
        let seed = fixture.gossip(0);
        nodes.push(fixture.start_node(index, vec![seed], true, None).await);
    }

    // The council reconciler grows the voter set to its cap of seven; the
    // remaining two nodes stay workers — the H1 subjects.
    wait_until(
        "seven voters",
        Duration::from_secs(60),
        async || match leader_of(&nodes, &all).await {
            Some(leader) => nodes[leader].voter_count() == 7,
            None => false,
        },
    )
    .await;

    // (a) EVERY node reports to the leader — including the two outside the
    // council, which can only have found it through the gossip directory.
    wait_until(
        "all 9 nodes reporting to the leader",
        Duration::from_secs(60),
        async || {
            leader_state(&nodes, &all)
                .await
                .is_some_and(|state| state.reports.len() == NODE_COUNT)
        },
    )
    .await;

    // The leader's committed membership is authoritative. Individual nodes'
    // metrics watches can trail that commit briefly, so counting each node's
    // local opinion here produced a false third "worker" in CI.
    let leader = wait_for_leader(&nodes, &all, Duration::from_secs(30)).await;
    let voter_ids = nodes[leader].voter_ids();
    let workers: Vec<&WiredNode> = nodes
        .iter()
        .filter(|node| !voter_ids.contains(&node.raft_id))
        .collect();
    assert_eq!(workers.len(), 2, "expected exactly two non-voter workers");
    let reports = nodes[leader].aggregated_rx.borrow().reports.clone();
    for worker in &workers {
        assert!(
            reports.contains_key(&NodeId::new(&worker.name)),
            "non-voter {} must appear in the leader's aggregated reports",
            worker.name
        );
    }

    // (b) A daemonset app converges everywhere — on the workers too, which
    // requires their placement reconcilers to reach the leader's API.
    write_through_leader(
        &nodes,
        &all,
        RaftRequest::AppSpec {
            app_id: AppId::new("web", "default"),
            spec: Box::new(daemonset_spec(600)),
        },
    )
    .await;

    wait_until(
        "web running on all 9 nodes (per aggregated reports)",
        Duration::from_secs(90),
        async || {
            leader_state(&nodes, &all)
                .await
                .is_some_and(|state| nodes_reporting_app(&state, "web") == NODE_COUNT)
        },
    )
    .await;

    // (c) Kill the leader. A new leader must emerge, every survivor must
    // re-point its reporting and placement polling at it (without restart),
    // and coverage must recover in the new epoch — pre-failover reports
    // cannot satisfy it (CP5), so what we see below is all fresh truth.
    let old_leader = wait_for_leader(&nodes, &all, Duration::from_secs(30)).await;
    kill(&nodes[old_leader]).await;
    let survivors: Vec<usize> = all
        .iter()
        .copied()
        .filter(|&index| index != old_leader)
        .collect();
    let new_leader = wait_for_leader(&nodes, &survivors, Duration::from_secs(60)).await;
    eprintln!(
        "killed leader {}, new leader {}",
        nodes[old_leader].name, nodes[new_leader].name
    );

    // Reporting coverage recovers on the NEW leader: all 8 survivors,
    // still running their daemonset instances.
    wait_until(
        "all 8 survivors reporting to the new leader with web running",
        Duration::from_secs(90),
        async || {
            leader_state(&nodes, &survivors).await.is_some_and(|state| {
                state.reports.len() == NODE_COUNT - 1
                    && nodes_reporting_app(&state, "web") == NODE_COUNT - 1
            })
        },
    )
    .await;

    // Placements still reconcile through the new leader: a fresh app
    // lands on every survivor, workers included.
    write_through_leader(
        &nodes,
        &survivors,
        RaftRequest::AppSpec {
            app_id: AppId::new("web2", "default"),
            spec: Box::new(daemonset_spec(600)),
        },
    )
    .await;

    wait_until(
        "web2 running on all 8 survivors after failover",
        Duration::from_secs(90),
        async || {
            leader_state(&nodes, &survivors)
                .await
                .is_some_and(|state| nodes_reporting_app(&state, "web2") == NODE_COUNT - 1)
        },
    )
    .await;

    fixture.root.cancel();
    // Give agents a moment to stop their sleep processes.
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// Ownership survives a real leader change even when the worker's local
/// placement journal is missing and it has missed the deletion entirely.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn lease_retirement_waits_for_paused_worker_across_leader_change() {
    use reliaburger::meat::{Placement, Resources, SchedulingDecision};
    use reliaburger::testkit::lease::{
        LeaseError, TestLease, cleanup_cluster_lease, now_unix_millis,
    };
    assert!(cluster_tests_enabled());
    let mut fixture = Fixture::new();
    let mut nodes = vec![fixture.start_node(200, vec![], false, None).await];
    for index in 201..203 {
        let seed = fixture.gossip(200);
        nodes.push(fixture.start_node(index, vec![seed], false, None).await);
    }
    let (leader, [follower, worker]) = settled_roles(&nodes).await;
    let all = [0, 1, 2];
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
                node_id: NodeId::new(&nodes[worker].name),
                resources: Resources::new(500, 1024 * 1024, 0),
                ordinal: 0,
            }],
        }),
    ] {
        assert!(!matches!(
            write_through_leader(&nodes, &all, request).await,
            CouncilResponse::Refused { .. }
        ));
    }
    wait_until(
        "worker running leased process",
        Duration::from_secs(30),
        async || {
            nodes[worker]
                .resolve("cleanup")
                .await
                .is_some_and(|view| view.total_backends == 1)
        },
    )
    .await;
    nodes[worker].reconciler.abort();
    let _ = (&mut nodes[worker].reconciler).await;
    let current = wait_for_leader(&nodes, &all, Duration::from_secs(30)).await;
    assert!(matches!(
        cleanup_cluster_lease(&nodes[current].council, "failover", None).await,
        Err(LeaseError::CleanupPending)
    ));
    let client = reqwest::Client::new();
    // A follower must never turn its possibly stale empty view into an
    // authoritative instruction to retire local work.
    let follower = if current == follower {
        leader
    } else {
        follower
    };
    assert_eq!(
        client
            .get(format!(
                "http://127.0.0.1:{}/v1/placements/{}",
                nodes[follower].api_port, nodes[worker].name
            ))
            .bearer_auth(RETIREMENT_SERVICE_TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    let old_leader = wait_for_leader(&nodes, &all, Duration::from_secs(30)).await;
    nodes[old_leader].shutdown.cancel();
    nodes[old_leader].council.shutdown().await.unwrap();
    let survivors: Vec<usize> = all
        .iter()
        .copied()
        .filter(|&index| index != old_leader)
        .collect();
    let successor = wait_for_leader(&nodes, &survivors, Duration::from_secs(60)).await;
    assert!(matches!(
        cleanup_cluster_lease(&nodes[successor].council, "failover", None).await,
        Err(LeaseError::CleanupPending)
    ));
    assert_eq!(
        nodes[successor].council.desired_state().await.test_leases["failover"]
            .placements
            .len(),
        1
    );
    // A fresh checkpoint cannot hide the still-live runtime from the leader's
    // retirement instruction. No local inventory is available on this restart.
    let checkpoint = tempfile::tempdir().unwrap();
    let resumed = spawn_placement_reconciler(
        nodes[worker].name.clone(),
        nodes[worker].metrics_rx.clone(),
        nodes[worker].directory_rx.clone(),
        2,
        Some(RETIREMENT_SERVICE_TOKEN.into()),
        nodes[worker].cmd_tx.clone(),
        nodes[worker].shutdown.clone(),
        reliaburger::cluster::ClusterHttp::plaintext(),
        Some(checkpoint.path().into()),
        reliaburger::config::node::RuntimeSection::default().stop_confirmation_timeout(),
    );
    wait_until(
        "confirmed worker retirement",
        Duration::from_secs(30),
        async || {
            let Some(current) = leader_of(&nodes, &survivors).await else {
                return false;
            };
            nodes[current].council.desired_state().await.test_leases["failover"]
                .placements
                .is_empty()
        },
    )
    .await;
    assert!(
        nodes[worker]
            .resolve("cleanup")
            .await
            .is_none_or(|view| view.total_backends == 0)
    );
    let current = wait_for_leader(&nodes, &survivors, Duration::from_secs(30)).await;
    cleanup_cluster_lease(&nodes[current].council, "failover", None)
        .await
        .unwrap();
    assert!(
        !nodes[current]
            .council
            .desired_state()
            .await
            .test_leases
            .contains_key("failover")
    );
    fixture.root.cancel();
    resumed.await.unwrap();
}

/// An unreachable worker's duties can be resolved by the operator without
/// admitting its old identity when gossip or membership catches up later.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
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
    let mut fixture = Fixture::new();
    let mut nodes = vec![
        fixture
            .start_node(300, vec![], false, Some(operator.token.clone()))
            .await,
    ];
    for index in 301..303 {
        let seed = fixture.gossip(300);
        nodes.push(
            fixture
                .start_node(index, vec![seed], false, Some(operator.token.clone()))
                .await,
        );
    }
    // The worker to decommission is one that doesn't lead; the other
    // survivor is the follower the operator's request goes through.
    let (leader, [follower, worker]) = settled_roles(&nodes).await;
    let survivors = [leader, follower];
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
                node_id: NodeId::new(&nodes[worker].name),
                resources: Resources::new(500, 1024 * 1024, 0),
                ordinal: 0,
            }],
        }),
    ] {
        assert!(!matches!(
            write_through_leader(&nodes, &[0, 1, 2], request).await,
            CouncilResponse::Refused { .. }
        ));
    }
    wait_until("worker workload", Duration::from_secs(30), async || {
        nodes[worker]
            .resolve("cleanup")
            .await
            .is_some_and(|view| view.total_backends == 1)
    })
    .await;
    // This is the operator's external shutdown, before the attestation.
    nodes[worker].shutdown.cancel();
    nodes[worker].council.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), nodes[worker].cmd_tx.closed())
        .await
        .unwrap();
    let current = wait_for_leader(&nodes, &survivors, Duration::from_secs(60)).await;
    assert!(matches!(
        cleanup_cluster_lease(&nodes[current].council, &lease.lease_id, None).await,
        Err(LeaseError::CleanupPending)
    ));
    let request = DecommissionRequest {
        node_id: nodes[worker].name.clone(),
        workloads_stopped: true,
        reason: "stopped for maintenance".into(),
    };
    // Use the real client and a follower to cover credential-preserving
    // forwarding (the API forwards whichever node leads by then).
    let client = reliaburger::relish::client::BunClient::new_with_token(
        &format!("http://127.0.0.1:{}", nodes[follower].api_port),
        Some(&operator.plaintext),
    );
    let record = client.decommission_node(&request).await.unwrap();
    assert_eq!(record.released_placements.get(&lease.lease_id), Some(&1));
    assert_eq!(client.decommission_node(&request).await.unwrap(), record);
    let current = wait_for_leader(&nodes, &survivors, Duration::from_secs(30)).await;
    cleanup_cluster_lease(&nodes[current].council, &lease.lease_id, None)
        .await
        .unwrap();
    wait_until(
        "retired voter removed on both survivors",
        Duration::from_secs(30),
        async || {
            settled_voters(&pick(&nodes, &survivors))
                .is_some_and(|voters| !voters.contains(&nodes[worker].raft_id))
        },
    )
    .await;
    let worker_raft = local(fixture.gossip_port(300 + worker) + 1);
    let current = wait_for_leader(&nodes, &survivors, Duration::from_secs(30)).await;
    assert!(
        nodes[current]
            .council
            .add_learner(
                nodes[worker].raft_id,
                reliaburger::council::CouncilNodeInfo::new(worker_raft, nodes[worker].name.clone())
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("retired")
    );
    let seeds = vec![fixture.gossip(300 + leader), fixture.gossip(300 + follower)];
    nodes.push(
        fixture
            .start_node(303, seeds, false, Some(operator.token.clone()))
            .await,
    );
    let replacement = 3;
    let members = [leader, follower, replacement];
    // Every survivor, not just the leader, must have applied the promotion
    // before the leader dies, or the election below can never be won.
    wait_until(
        "fresh replacement voter applied by every member",
        Duration::from_secs(60),
        async || {
            settled_voters(&pick(&nodes, &members)).is_some_and(|voters| {
                voters == members.iter().map(|&index| nodes[index].raft_id).collect()
            })
        },
    )
    .await;
    let old_leader = wait_for_leader(&nodes, &members, Duration::from_secs(30)).await;
    nodes[old_leader].shutdown.cancel();
    nodes[old_leader].council.shutdown().await.unwrap();
    let remaining: Vec<usize> = members
        .iter()
        .copied()
        .filter(|&index| index != old_leader)
        .collect();
    let successor = wait_for_leader(&nodes, &remaining, Duration::from_secs(60)).await;
    let state = nodes[successor].council.desired_state().await;
    assert_eq!(
        state.security_state.crl.retired_nodes[&nodes[worker].name],
        record
    );
    assert!(!state.test_leases.contains_key(&lease.lease_id));
    fixture.root.cancel();
}

/// POST `manifest` to `node`'s apply route as `bearer`, answering the
/// status and body, or `None` when the node is gone.
async fn post_apply(node: &WiredNode, bearer: &str, manifest: &str) -> Option<(u16, String)> {
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{}/v1/apply", node.api_port))
        .bearer_auth(bearer)
        .body(manifest.to_string())
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .ok()?;
    let status = response.status().as_u16();
    Some((status, response.text().await.unwrap_or_default()))
}

/// #1117: a leader killed while it applies apps under a policy leaves the
/// policy in force. The new leader holds the same policy set at the same
/// generation, refuses an app that breaks it, and accepts the interrupted
/// apply when it's sent again.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn a_leader_killed_during_an_apply_under_policy_leaves_the_policy_enforced() {
    assert!(cluster_tests_enabled());
    let operator = reliaburger::sesame::token::create_token(
        "operator",
        reliaburger::sesame::types::ApiRole::Admin,
        Default::default(),
        None,
    )
    .unwrap();
    let mut fixture = Fixture::new();
    let mut nodes = vec![
        fixture
            .start_node(400, vec![], false, Some(operator.token.clone()))
            .await,
    ];
    for index in 401..403 {
        let seed = fixture.gossip(400);
        nodes.push(
            fixture
                .start_node(index, vec![seed], false, Some(operator.token.clone()))
                .await,
        );
    }
    let (leader, survivors) = settled_roles(&nodes).await;

    let policy = "[policy.no-latest]\ndeny = \"app.image.endsWith(':latest')\"\n";
    let (status, body) = post_apply(&nodes[leader], &operator.plaintext, policy)
        .await
        .expect("the leader answers");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("policy no-latest: committed"), "{body}");

    // Twenty pinned apps, applied while the leader is killed.
    let apps: String = (0..20)
        .map(|index| format!("[app.web-{index}]\nimage = 'web:v{index}'\n"))
        .collect();
    let interrupted = {
        let api_port = nodes[leader].api_port;
        let bearer = operator.plaintext.clone();
        let apps = apps.clone();
        tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("http://127.0.0.1:{api_port}/v1/apply"))
                .bearer_auth(bearer)
                .body(apps)
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .ok()
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    kill(&nodes[leader]).await;
    let _ = interrupted.await;

    let successor = wait_for_leader(&nodes, &survivors, Duration::from_secs(90)).await;
    let state = nodes[successor].council.desired_state().await;
    assert!(state.policies.contains_key("no-latest"));
    assert_eq!(state.policy_generation, 1);

    // The new leader enforces the same set.
    let mut refused = None;
    wait_until(
        "the new leader judges an apply",
        Duration::from_secs(60),
        async || {
            let leader = wait_for_leader(&nodes, &survivors, Duration::from_secs(30)).await;
            refused = post_apply(
                &nodes[leader],
                &operator.plaintext,
                "[app.bad]\nimage = 'bad:latest'\n",
            )
            .await;
            refused.as_ref().is_some_and(|(status, _)| *status != 503)
        },
    )
    .await;
    let (status, body) = refused.unwrap();
    assert_eq!(status, 422, "{body}");
    assert!(
        body.contains("policy no-latest denies app default/bad"),
        "{body}"
    );

    // Sending the interrupted apply again commits every app, fenced on the
    // generation the new leader holds.
    let leader = wait_for_leader(&nodes, &survivors, Duration::from_secs(30)).await;
    let (status, body) = post_apply(&nodes[leader], &operator.plaintext, &apps)
        .await
        .expect("the new leader answers");
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("\"Error\""), "{body}");
    let state = nodes[leader].council.desired_state().await;
    for index in 0..20 {
        assert!(
            state
                .apps
                .contains_key(&AppId::new(format!("web-{index}"), "default"))
        );
    }
    assert!(!state.apps.contains_key(&AppId::new("bad", "default")));
    fixture.root.cancel();
}

// --- The 0.2.4 exit test (#1126) and break-glass on a cluster (#1311) -------

/// The four policies platform teams ask for most, exactly as published.
const PLATFORM_POLICIES: &str = include_str!("../examples/policies/platform.toml");

fn admin_token(name: &str) -> reliaburger::sesame::token::CreatedToken {
    reliaburger::sesame::token::create_token(
        name,
        reliaburger::sesame::types::ApiRole::Admin,
        Default::default(),
        None,
    )
    .unwrap()
}

/// Give every node's API `token`, as `relish token create` would.
async fn grant(nodes: &[WiredNode], token: &reliaburger::sesame::types::ApiToken) {
    for node in nodes {
        if let Some(store) = &node.token_store {
            store.write().await.push(token.clone());
        }
    }
}

/// POST `manifest` to `/v1/apply` on whichever node in `among` leads, until
/// one answers with something other than "no leader".
async fn apply_on_leader(
    nodes: &[WiredNode],
    among: &[usize],
    bearer: &str,
    manifest: &str,
) -> (u16, String) {
    let mut answer = None;
    wait_until(
        "a leader answers an apply",
        Duration::from_secs(90),
        async || {
            let leader = wait_for_leader(nodes, among, Duration::from_secs(60)).await;
            answer = post_apply(&nodes[leader], bearer, manifest).await;
            answer.as_ref().is_some_and(|(status, _)| *status != 503)
        },
    )
    .await;
    answer.unwrap()
}

/// POST `/v1/policies/{name}/delete` to `node` as `bearer`.
async fn delete_policy(node: &WiredNode, bearer: &str, name: &str) -> Option<(u16, String)> {
    let response = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{}/v1/policies/{name}/delete",
            node.api_port
        ))
        .bearer_auth(bearer)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .ok()?;
    let status = response.status().as_u16();
    Some((status, response.text().await.unwrap_or_default()))
}

fn run_git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "exit-test")
        .env("GIT_AUTHOR_EMAIL", "exit-test@example.com")
        .env("GIT_COMMITTER_NAME", "exit-test")
        .env("GIT_COMMITTER_EMAIL", "exit-test@example.com")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn commit(repository: &std::path::Path, file: &str, contents: &str, message: &str) {
    std::fs::write(repository.join(file), contents).unwrap();
    run_git(repository, &["add", "."]);
    run_git(repository, &["commit", "-q", "-m", message]);
}

/// Run the GitOps loop on every node, as `bun --cluster` does: only the
/// leader syncs, so a new leader carries on where a killed one stopped.
/// Each loop stops with its node. Hold the returned senders, the webhook
/// channels, until the test ends.
fn start_gitops(
    nodes: &[WiredNode],
    repository: &std::path::Path,
    data: &std::path::Path,
) -> Vec<tokio::sync::mpsc::Sender<()>> {
    nodes
        .iter()
        .map(|node| {
            let (webhook_tx, webhook_rx) = tokio::sync::mpsc::channel(4);
            reliaburger::lettuce::runner::spawn_gitops_sync(
                std::sync::Arc::clone(&node.council),
                Some(reliaburger::lettuce::types::GitOpsConfig {
                    repo: repository.to_string_lossy().to_string(),
                    branch: "main".to_string(),
                    path: "/".to_string(),
                    poll_interval_secs: 1,
                    require_signed_commits: false,
                    trusted_signing_keys: vec![],
                    webhook_secret: None,
                    webhook_rate_limit: 10,
                }),
                webhook_rx,
                data.join(&node.name),
                node.shutdown.clone(),
                None,
            );
            webhook_tx
        })
        .collect()
}

/// The `TEAM` a stored app runs with, if any.
fn team_of(state: &reliaburger::council::types::DesiredState, app: &AppId) -> Option<String> {
    let spec = serde_json::to_value(state.apps.get(app)?).ok()?;
    spec["env"]["TEAM"].as_str().map(str::to_string)
}

/// The leader's last GitOps error, read from whichever node in `among`
/// leads, and whether `app` is in its desired state.
async fn gitops_outcome(nodes: &[WiredNode], among: &[usize], app: &AppId) -> (bool, String) {
    let Some(leader) = leader_of(nodes, among).await else {
        return (false, String::new());
    };
    let state = nodes[leader].council.desired_state().await;
    let error = state
        .gitops_sync_state
        .as_ref()
        .and_then(|sync| sync.last_error.clone())
        .unwrap_or_default();
    (state.apps.contains_key(app), error)
}

/// #1126, the exit test's first two items: the four platform policies,
/// written in TOML and applied unchanged, enforced through both `relish
/// apply` and GitOps, and still enforced, at the same generation, by the
/// leader that takes over when the first is killed during an apply.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn the_platform_policies_hold_through_apply_gitops_and_a_leader_kill() {
    assert!(cluster_tests_enabled());
    let operator = admin_token("operator");
    let mut fixture = Fixture::new();
    let mut nodes = vec![
        fixture
            .start_node(500, vec![], false, Some(operator.token.clone()))
            .await,
    ];
    for index in 501..503 {
        let seed = fixture.gossip(500);
        nodes.push(
            fixture
                .start_node(index, vec![seed], false, Some(operator.token.clone()))
                .await,
        );
    }
    let (leader, followers) = settled_roles(&nodes).await;
    let admin = operator.plaintext.as_str();

    let (status, body) = post_apply(&nodes[leader], admin, "[namespace.prod]\n")
        .await
        .expect("the leader answers");
    assert_eq!(status, 200, "{body}");
    let (status, body) = post_apply(&nodes[leader], admin, PLATFORM_POLICIES)
        .await
        .expect("the leader answers");
    assert_eq!(status, 200, "{body}");
    for policy in [
        "no-latest",
        "prod-needs-memory-limits",
        "team-env",
        "trusted-registries",
    ] {
        assert!(
            body.contains(&format!("policy {policy}: committed")),
            "{body}"
        );
    }
    let generation = nodes[leader]
        .council
        .desired_state()
        .await
        .policy_generation;

    // Through `relish apply`, sent to a follower, which passes the apply
    // and the caller's token to the leader: each rule refuses its case.
    let refusals = [
        (
            "[app.web]\nnamespace = 'prod'\nimage = 'registry.example.com/web:1.4'\n",
            "policy prod-needs-memory-limits denies app prod/web",
        ),
        (
            "[app.web]\nimage = 'registry.example.com/web:latest'\n",
            "policy no-latest denies app default/web",
        ),
        (
            "[app.web]\nimage = 'docker.io/library/nginx:1.27'\n",
            "policy trusted-registries denies app default/web",
        ),
        (
            "[app.web]\nimage = 'registry.example.com/web:1.4'\nenv = { TEAM = '' }\n",
            "policy team-env denies app default/web",
        ),
    ];
    for (manifest, denial) in refusals {
        let (status, body) = post_apply(&nodes[followers[0]], admin, manifest)
            .await
            .expect("the follower answers");
        assert_eq!(status, 422, "{manifest}: {body}");
        assert!(body.contains(denial), "{denial}: {body}");
    }
    let (status, body) = post_apply(
        &nodes[followers[1]],
        admin,
        "[app.web]\nnamespace = 'prod'\nimage = 'registry.example.com/web:1.4'\nmemory = '256Mi-1Gi'\n",
    )
    .await
    .expect("the follower answers");
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("policy team-env fills env.TEAM = \\\"platform\\\""),
        "{body}"
    );
    let state = nodes[leader].council.desired_state().await;
    let web = AppId::new("web", "prod");
    assert_eq!(team_of(&state, &web).as_deref(), Some("platform"));
    assert!(
        state.declared_apps.contains_key(&web),
        "the declared spec is kept beside the filled one"
    );

    // Through GitOps: a commit with three apps lands the one that passes
    // and names the policy that refused each of the others.
    let repository = tempfile::tempdir().unwrap();
    run_git(repository.path(), &["init", "-q", "-b", "main"]);
    commit(
        repository.path(),
        "apps.toml",
        "[app.api]\nnamespace = 'prod'\nimage = 'ghcr.io/example/api:2.0'\nmemory = '128Mi'\n\n\
         [app.mirror]\nimage = 'docker.io/library/nginx:1.27'\n\n\
         [app.untagged]\nimage = 'registry.example.com/untagged'\n",
        "three apps",
    );
    let gitops_data = tempfile::tempdir().unwrap();
    let _webhooks = start_gitops(&nodes, repository.path(), gitops_data.path());
    let api = AppId::new("api", "prod");
    let mut last_error = String::new();
    wait_until(
        "GitOps reports its partial result",
        Duration::from_secs(60),
        async || {
            let (landed, error) = gitops_outcome(&nodes, &[0, 1, 2], &api).await;
            last_error = error;
            landed && last_error.contains("untagged")
        },
    )
    .await;
    assert!(
        last_error.contains("app.default/mirror: denied by policy trusted-registries"),
        "{last_error}"
    );
    assert!(
        last_error.contains("app.default/untagged: denied by policy no-latest"),
        "{last_error}"
    );
    let leader = wait_for_leader(&nodes, &[0, 1, 2], Duration::from_secs(30)).await;
    let state = nodes[leader].council.desired_state().await;
    assert_eq!(team_of(&state, &api).as_deref(), Some("platform"));
    assert!(!state.apps.contains_key(&AppId::new("mirror", "default")));
    let survivors: Vec<usize> = (0..3).filter(|&index| index != leader).collect();

    // Kill the leader while it applies twenty apps under the policies.
    let apps: String = (0..20)
        .map(|index| {
            format!("[app.batch-{index}]\nimage = 'registry.example.com/batch:v{index}'\n")
        })
        .collect();
    let interrupted = {
        let api_port = nodes[leader].api_port;
        let bearer = operator.plaintext.clone();
        let apps = apps.clone();
        tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("http://127.0.0.1:{api_port}/v1/apply"))
                .bearer_auth(bearer)
                .body(apps)
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .ok()
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    kill(&nodes[leader]).await;
    let _ = interrupted.await;

    // The new leader holds the same four policies at the same generation.
    let successor = wait_for_leader(&nodes, &survivors, Duration::from_secs(90)).await;
    let state = nodes[successor].council.desired_state().await;
    assert_eq!(state.policies.len(), 4, "{:?}", state.policies.keys());
    assert_eq!(state.policy_generation, generation);

    // It refuses through `relish apply`...
    let (status, body) = apply_on_leader(
        &nodes,
        &survivors,
        admin,
        "[app.bad]\nimage = 'registry.example.com/bad:latest'\n",
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body.contains("policy no-latest denies app default/bad"),
        "{body}"
    );

    // ...and through GitOps, whose loop on the new leader picks up the
    // next commit.
    commit(
        repository.path(),
        "more.toml",
        "[app.worker]\nimage = 'registry.example.com/worker:3'\n\n\
         [app.drifter]\nimage = 'registry.example.com/drifter:latest'\n",
        "two more apps",
    );
    let worker = AppId::new("worker", "default");
    wait_until(
        "the new leader's GitOps sync",
        Duration::from_secs(60),
        async || {
            let (landed, error) = gitops_outcome(&nodes, &survivors, &worker).await;
            last_error = error;
            landed && last_error.contains("drifter")
        },
    )
    .await;
    assert!(
        last_error.contains("app.default/drifter: denied by policy no-latest"),
        "{last_error}"
    );

    // Sending the interrupted apply again commits every app, each with
    // its team filled in.
    let (status, body) = apply_on_leader(&nodes, &survivors, admin, &apps).await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("\"Error\""), "{body}");
    let leader = wait_for_leader(&nodes, &survivors, Duration::from_secs(30)).await;
    let state = nodes[leader].council.desired_state().await;
    for index in 0..20 {
        let app = AppId::new(format!("batch-{index}"), "default");
        assert_eq!(
            team_of(&state, &app).as_deref(),
            Some("platform"),
            "{app:?}"
        );
    }
    assert!(!state.apps.contains_key(&AppId::new("bad", "default")));
    assert!(!state.apps.contains_key(&AppId::new("drifter", "default")));
    fixture.root.cancel();
}

/// #1311 item 3 and the exit test's break-glass: a policy that denies
/// everything refuses every app and job, for admins too, but never a
/// policy write. With its leader killed, an admin removes it through a
/// follower of the new leader, and applies work again. A deployer can't
/// remove it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn an_admin_removes_a_policy_that_denies_everything_through_any_node() {
    assert!(cluster_tests_enabled());
    let operator = admin_token("operator");
    let mut fixture = Fixture::new();
    let mut nodes = vec![
        fixture
            .start_node(600, vec![], false, Some(operator.token.clone()))
            .await,
    ];
    for index in 601..603 {
        let seed = fixture.gossip(600);
        nodes.push(
            fixture
                .start_node(index, vec![seed], false, Some(operator.token.clone()))
                .await,
        );
    }
    let deployer = reliaburger::sesame::token::create_token(
        "ci",
        reliaburger::sesame::types::ApiRole::Deployer,
        Default::default(),
        None,
    )
    .unwrap();
    grant(&nodes, &deployer.token).await;
    let (leader, followers) = settled_roles(&nodes).await;
    let admin = operator.plaintext.as_str();

    let broken = "[policy.broken]\napplies_to = 'both'\ndeny = 'true'\nmessage = 'a typo that denies everything'\n";
    let (status, body) = post_apply(&nodes[followers[0]], admin, broken)
        .await
        .expect("the follower answers");
    assert_eq!(status, 200, "{body}");

    let app = "[app.web]\nimage = 'registry.example.com/web:1'\n";
    let job = "[job.report]\nimage = 'registry.example.com/report:1'\ncommand = ['report']\n";
    for (bearer, manifest, denial) in [
        (
            deployer.plaintext.as_str(),
            app,
            "policy broken denies app default/web",
        ),
        (admin, app, "policy broken denies app default/web"),
        (admin, job, "policy broken denies job default/report"),
    ] {
        let (status, body) = post_apply(&nodes[followers[1]], bearer, manifest)
            .await
            .expect("the follower answers");
        assert_eq!(status, 422, "{manifest}: {body}");
        assert!(body.contains(denial), "{denial}: {body}");
    }
    // A policy never judges a policy, so policy writes still work.
    let (status, body) = post_apply(
        &nodes[followers[1]],
        admin,
        "[policy.harmless]\naction = 'warn'\ndeny = 'false'\n",
    )
    .await
    .expect("the follower answers");
    assert_eq!(status, 200, "{body}");

    kill(&nodes[leader]).await;
    let successor = wait_for_leader(&nodes, &followers, Duration::from_secs(90)).await;
    let follower = followers
        .into_iter()
        .find(|&index| index != successor)
        .expect("two survivors");

    // Only an admin may remove it.
    let (status, body) = delete_policy(&nodes[follower], &deployer.plaintext, "broken")
        .await
        .expect("the follower answers");
    assert_eq!(status, 403, "{body}");
    let mut answer = None;
    wait_until(
        "the policy delete reaches the new leader",
        Duration::from_secs(60),
        async || {
            answer = delete_policy(&nodes[follower], admin, "broken").await;
            answer.as_ref().is_some_and(|(status, _)| *status != 503)
        },
    )
    .await;
    let (status, body) = answer.unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"deleted\""), "{body}");

    let (status, body) =
        apply_on_leader(&nodes, &[successor, follower], &deployer.plaintext, app).await;
    assert_eq!(status, 200, "{body}");
    for index in [successor, follower] {
        let node = &nodes[index];
        wait_until(
            "both survivors apply the delete",
            Duration::from_secs(30),
            async || {
                let state = node.council.desired_state().await;
                !state.policies.contains_key("broken")
                    && state.policies.contains_key("harmless")
                    && state.apps.contains_key(&AppId::new("web", "default"))
            },
        )
        .await;
    }
    fixture.root.cancel();
}

// The example controller, compiled in as the portable suite does: it
// imports nothing from `reliaburger`, only HTTP and JSON.
#[allow(dead_code)]
#[path = "../examples/controller/main.rs"]
mod controller;

const POSTGRES_PAIR: &str = include_str!("../examples/controller/postgres-pair.toml");

/// The published Postgres pair template with a stand-in for Postgres. The
/// fixture's nodes run each container's command as a host process and pull
/// nothing, so the image can't run, and a host process can't enforce a
/// memory limit, so the template's limit has to go too. The stand-in keeps
/// the rest of the shape, one instance per member, a port, pinned to its
/// rack, owned by the controller, and runs `sleep`. Its image comes from a
/// registry the platform policies trust, so the controller's writes pass
/// them.
fn stand_in_pair() -> String {
    let image = "image = \"postgres:${version}\"\n";
    let memory = "memory = \"256Mi-1Gi\"\n";
    assert!(
        POSTGRES_PAIR.contains(image),
        "the template's image line moved"
    );
    assert!(
        POSTGRES_PAIR.contains(memory),
        "the template's memory line moved"
    );
    POSTGRES_PAIR.replace(memory, "").replace(
        image,
        "image = \"registry.example.com/postgres:${version}\"\ncommand = [\"/bin/sleep\", \"600\"]\n",
    )
}

/// The node, by name, whose report lists `app` running, as the leader in
/// `among` sees it.
async fn running_on(nodes: &[WiredNode], among: &[usize], app: &str) -> Option<String> {
    node_running_app(&leader_state(nodes, among).await?, app)
}

/// How often the test's controller checks. The example checks every five
/// seconds; with `CHECK_THRESHOLD` misses in a row before a failover, a
/// fresh app gets about fifteen seconds to be placed and started, which a
/// new leader's learning period needs.
const CHECK_INTERVAL: Duration = Duration::from_secs(2);
const CHECK_THRESHOLD: u32 = 7;

/// Wait one interval, then check once with the controller, logging what it
/// decided.
async fn check_once(
    controller: &mut controller::Controller,
    api: &controller::Api,
) -> Option<controller::Decision> {
    tokio::time::sleep(CHECK_INTERVAL).await;
    match controller.reconcile(api).await {
        Ok(decision) => {
            eprintln!("controller: {decision:?}");
            Some(decision)
        }
        Err(error) => {
            eprintln!("controller: {error}");
            None
        }
    }
}

/// #1126, the exit test's third item: a template plus a small external
/// controller that fails over a replica, against the public API only.
///
/// The controller is `examples/controller/main.rs`, driving the template in
/// `examples/controller/postgres-pair.toml`, with a stand-in for Postgres
/// (see [`stand_in_pair`]). Its token is scoped to `pg`, and the platform
/// policies are in force. It starts `db-a` on rack `a`; when the node on
/// rack `a` is killed, `db-a` has nowhere to run, and after
/// `CHECK_THRESHOLD` missed checks the controller applies `db-b`, which
/// runs on rack `b`, and deletes `db-a`.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn a_controller_fails_a_templated_database_pair_over_when_its_node_dies() {
    assert!(cluster_tests_enabled());
    let operator = admin_token("operator");
    let mut fixture = Fixture::new();
    let mut nodes = vec![
        fixture
            .start_racked_node(700, vec![], operator.token.clone(), "a")
            .await,
    ];
    for (index, rack) in [(701, "b"), (702, "c")] {
        let seed = fixture.gossip(700);
        nodes.push(
            fixture
                .start_racked_node(index, vec![seed], operator.token.clone(), rack)
                .await,
        );
    }
    let scoped = reliaburger::sesame::token::create_token(
        "db-failover",
        reliaburger::sesame::types::ApiRole::Deployer,
        reliaburger::sesame::types::TokenScope {
            apps: None,
            namespaces: Some(vec!["pg".to_string()]),
        },
        None,
    )
    .unwrap();
    grant(&nodes, &scoped.token).await;
    settled_roles(&nodes).await;
    let (status, body) = apply_on_leader(
        &nodes,
        &[0, 1, 2],
        &operator.plaintext,
        &format!("[namespace.pg]\n{PLATFORM_POLICIES}"),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // The controller talks to the node on rack b, which survives.
    let api = controller::Api::new(controller::Settings {
        endpoint: format!("http://127.0.0.1:{}", nodes[1].api_port),
        token: scoped.plaintext.clone(),
        ca_cert: None,
        name: "db-failover".to_string(),
        namespace: "pg".to_string(),
        manifest: stand_in_pair(),
        threshold: CHECK_THRESHOLD,
    })
    .unwrap();
    let mut db_failover = controller::Controller::default();

    wait_until(
        "the controller starts db-a, running on rack a",
        Duration::from_secs(60),
        async || {
            let decision = check_once(&mut db_failover, &api).await;
            matches!(decision, Some(controller::Decision::Wait { .. }))
                && running_on(&nodes, &[0, 1, 2], "db-a").await.as_deref()
                    == Some(nodes[0].name.as_str())
        },
    )
    .await;

    kill(&nodes[0]).await;
    let db_a = AppId::new("db-a", "pg");
    let db_b = AppId::new("db-b", "pg");
    wait_until(
        "the controller fails over to db-b, running on rack b",
        Duration::from_secs(150),
        async || {
            check_once(&mut db_failover, &api).await;
            let state = nodes[1].council.desired_state().await;
            !state.apps.contains_key(&db_a)
                && state.apps.contains_key(&db_b)
                && running_on(&nodes, &[1, 2], "db-b").await.as_deref()
                    == Some(nodes[1].name.as_str())
        },
    )
    .await;

    let state = nodes[1].council.desired_state().await;
    let spec = &state.apps[&db_b];
    assert_eq!(spec.owner.as_deref(), Some("db-failover"));
    assert_eq!(
        spec.provenance.as_ref().map(|p| p.template.as_str()),
        Some("postgres-member")
    );
    assert_eq!(
        team_of(&state, &db_b).as_deref(),
        Some("platform"),
        "the controller's write is judged by the cluster's policies"
    );
    // Steady again: the standby runs, so the next check changes nothing.
    assert!(matches!(
        check_once(&mut db_failover, &api).await,
        Some(controller::Decision::Wait { .. })
    ));
    fixture.root.cancel();
}

/// #1399: four clients take, renew, read and release test leases through
/// all three nodes while the leader is killed part-way, recording every
/// operation; the history must check as linearizable. The same test with
/// a deliberately broken read (a follower answering from its own copy
/// without confirming leadership) fails the check: see "Checking the
/// history, not just the invariants" in docs/testing.md.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1, a multi-core host and python3; run with make test-cluster"]
async fn a_lease_history_through_a_leader_kill_is_linearizable() {
    assert!(cluster_tests_enabled());
    let operator = admin_token("history");
    let mut fixture = Fixture::new();
    let leases = || NodePlacement {
        test_operations: true,
        ..NodePlacement::default()
    };
    let mut nodes = vec![
        fixture
            .start_node_with(500, vec![], false, Some(operator.token.clone()), leases())
            .await,
    ];
    for index in 501..503 {
        let seed = fixture.gossip(500);
        nodes.push(
            fixture
                .start_node_with(
                    index,
                    vec![seed],
                    false,
                    Some(operator.token.clone()),
                    leases(),
                )
                .await,
        );
    }
    let (leader, _) = settled_roles(&nodes).await;

    let token_file = fixture.data.path().join("history-token");
    std::fs::write(&token_file, &operator.plaintext).unwrap();
    let history = fixture.data.path().join("council-history.jsonl");
    let scripts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/release");
    let mut client = tokio::process::Command::new("python3");
    client
        .arg(scripts.join("council_history.py"))
        .args(["run", "--clients", "4", "--pause", "0.05", "--timeout", "5"])
        .args(["--duration", "40", "--history"])
        .arg(&history)
        .arg("--token-file")
        .arg(&token_file)
        .kill_on_drop(true);
    for node in &nodes {
        client
            .arg("--node")
            .arg(format!("{}={}", node.name, node.api_port));
    }
    let mut client = client.spawn().expect("python3 runs the history client");

    tokio::time::sleep(Duration::from_secs(15)).await;
    kill(&nodes[leader]).await;
    let finished = tokio::time::timeout(Duration::from_secs(120), client.wait())
        .await
        .expect("the history client stops after its duration")
        .unwrap();
    assert!(finished.success(), "history client exited with {finished}");

    let checked = tokio::process::Command::new("python3")
        .arg(scripts.join("linearizability.py"))
        .arg("check")
        .arg(&history)
        .output()
        .await
        .unwrap();
    let result: serde_json::Value = serde_json::from_slice(&checked.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&checked.stderr)));
    assert_eq!(result["verdict"], "linearizable", "{result:#}");
    assert!(
        result["checked"].as_u64().unwrap_or(0) > 200,
        "too few operations checked: {result:#}"
    );
}
