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
            membership: MembershipSource::Directory,
            service_identity: Some(RETIREMENT_SERVICE_TOKEN.into()),
            operator_token: operator,
            fault_injection: false,
            labels: Default::default(),
            keep_data_dir: false,
            task_arrays: None,
            task_arrays_on_agent_budget: None,
        })
        .await
    }
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
        image = "proc-grill:image-ignored"
        command = ["sleep", "600"]
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
