//! Acceptance tests for council membership self-healing (Phase 12b.2,
//! findings H2/D2).
//!
//! An in-process cluster harness: real `CouncilNode`s over the in-memory
//! Raft router, with gossip faked through the reconciler's membership watch
//! channel so tests control exactly who looks alive and when. Gated behind
//! `RELIABURGER_CLUSTER_TESTS=1` because the hysteresis windows make each
//! test run for several seconds.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use reliaburger::cluster::identity::raft_id_from_name;
use reliaburger::cluster::runtime::{
    CouncilReconcilerConfig, spawn_council_reconciler_with_config,
};
use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::selection::CouncilSelectionConfig;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo, RaftRequest};
use reliaburger::meat::NodeId;
use reliaburger::mustard::membership::MembershipSnapshot;
use reliaburger::mustard::state::NodeState;

fn cluster_tests_enabled() -> bool {
    std::env::var("RELIABURGER_CLUSTER_TESTS").is_ok()
}

const NAMES: [&str; 5] = ["node-1", "node-2", "node-3", "node-4", "node-5"];

fn rid(index: usize) -> u64 {
    raft_id_from_name(NAMES[index])
}

fn gossip_addr(index: usize) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9443 + 2 * index as u16))
}

fn node_info(index: usize) -> CouncilNodeInfo {
    // Raft address = gossip address + the port offset of 1 used below.
    CouncilNodeInfo::new(
        SocketAddr::from(([127, 0, 0, 1], 9444 + 2 * index as u16)),
        NAMES[index].to_string(),
    )
}

/// A gossip view of node `index`: alive, warm (backdated `first_seen` so the
/// candidate alive window is already satisfied at test start).
fn member(index: usize, now: Instant) -> MembershipSnapshot {
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

fn fast_council_config() -> CouncilConfig {
    CouncilConfig {
        heartbeat_interval_ms: 50,
        election_timeout_min_ms: 200,
        election_timeout_max_ms: 400,
        snapshot_threshold: 1000,
        max_in_snapshot_log_to_keep: 500,
    }
}

/// Sub-second hysteresis so the acceptance tests finish in seconds.
fn heal_reconciler_config() -> CouncilReconcilerConfig {
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

struct Harness {
    nodes: Vec<Arc<CouncilNode>>,
    router: InMemoryRaftRouter,
    membership_tx: watch::Sender<Vec<MembershipSnapshot>>,
    shutdown: CancellationToken,
}

impl Harness {
    /// Five council nodes on the in-memory router; the first three form the
    /// initial voter set, the last two are spares. Reconcilers run on every
    /// node, exactly as in production.
    async fn start() -> Self {
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

        let now = Instant::now();
        let snapshot: Vec<MembershipSnapshot> = (0..NAMES.len()).map(|i| member(i, now)).collect();
        let (membership_tx, membership_rx) = watch::channel(snapshot);

        let shutdown = CancellationToken::new();
        for (index, node) in nodes.iter().enumerate() {
            spawn_council_reconciler_with_config(
                Arc::clone(node),
                membership_rx.clone(),
                1,
                rid(index),
                node_info(index),
                heal_reconciler_config(),
                shutdown.clone(),
            );
        }

        Self {
            nodes,
            router,
            membership_tx,
            shutdown,
        }
    }

    /// Index of the elected leader among the initial voters.
    async fn wait_for_leader(&self) -> usize {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            for index in 0..3 {
                if let Some(leader) = self.nodes[index].current_leader().await
                    && let Some(pos) = (0..3).find(|i| rid(*i) == leader)
                {
                    return pos;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no leader elected within 5s"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn voters_of(&self, index: usize) -> BTreeSet<u64> {
        self.nodes[index]
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .voter_ids()
            .collect()
    }

    fn membership_nodes_of(&self, index: usize) -> BTreeSet<u64> {
        self.nodes[index]
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .nodes()
            .map(|(id, _)| *id)
            .collect()
    }

    /// Publish a gossip view containing exactly the given node indices.
    fn set_gossip(&self, alive: &[usize]) {
        let now = Instant::now();
        let snapshot: Vec<MembershipSnapshot> = alive.iter().map(|i| member(*i, now)).collect();
        self.membership_tx.send(snapshot).unwrap();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Poll `cond` every 50ms until it holds or `timeout` elapses.
async fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cond()
}

/// Kill one voter: within a bounded time the council is back to three
/// healthy voters (the older spare promoted, the dead voter evicted), the
/// cluster commits writes throughout, and the leader is never removed.
#[tokio::test]
async fn killed_voter_is_replaced_by_healthy_spare() {
    if !cluster_tests_enabled() {
        eprintln!("skipping cluster test (set RELIABURGER_CLUSTER_TESTS=1)");
        return;
    }

    let harness = Harness::start().await;
    let leader = harness.wait_for_leader().await;
    let leader_id = rid(leader);

    // Settle: with the council cap at 3 the spares must stay spares.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let initial: BTreeSet<u64> = (0..3).map(rid).collect();
    assert_eq!(harness.voters_of(leader), initial);

    // A writer that hammers the council for the whole exercise, and a
    // monitor that snapshots the voter set as membership changes land.
    let writer_node = Arc::clone(&harness.nodes[leader]);
    let writer_stop = CancellationToken::new();
    let writer = {
        let stop = writer_stop.clone();
        tokio::spawn(async move {
            let mut failures = 0u32;
            let mut writes = 0u32;
            while !stop.is_cancelled() {
                let request = RaftRequest::ConfigSet {
                    key: "heartbeat".to_string(),
                    value: writes.to_string(),
                };
                match writer_node.write(request).await {
                    Ok(_) => writes += 1,
                    Err(e) => {
                        eprintln!("write failed during self-healing: {e}");
                        failures += 1;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            (writes, failures)
        })
    };

    // Kill a non-leader voter: its Raft dies and gossip reaps it.
    let victim = if leader == 2 { 1 } else { 2 };
    harness.nodes[victim].shutdown().await.unwrap();
    let alive: Vec<usize> = (0..5).filter(|i| *i != victim).collect();
    harness.set_gossip(&alive);

    // The healed council: both survivors plus the older spare (node-4).
    let survivor = if victim == 2 { 1 } else { 2 };
    let expected: BTreeSet<u64> = BTreeSet::from([rid(0), rid(survivor), rid(3)]);

    let healed = wait_until(Duration::from_secs(15), || {
        // The leader keeps its seat through every intermediate config.
        let voters = harness.voters_of(leader);
        assert!(
            voters.contains(&leader_id),
            "leader {leader_id} removed mid-change: {voters:?}"
        );
        voters == expected
    })
    .await;
    assert!(
        healed,
        "council did not heal to {expected:?}; got {:?}",
        harness.voters_of(leader)
    );

    // The dead voter is gone entirely — not lingering as a learner.
    let evicted = wait_until(Duration::from_secs(5), || {
        !harness.membership_nodes_of(leader).contains(&rid(victim))
    })
    .await;
    assert!(evicted, "dead voter still in membership");

    // Leadership never moved.
    assert_eq!(
        harness.nodes[leader].current_leader().await,
        Some(leader_id)
    );

    // Writes flowed throughout the whole exercise.
    writer_stop.cancel();
    let (writes, failures) = writer.await.unwrap();
    assert!(writes > 0, "writer made no progress");
    assert_eq!(failures, 0, "writes failed during self-healing");

    // Sanity: every healed voter is someone gossip says is alive.
    assert!(
        expected
            .iter()
            .all(|id| alive.iter().any(|i| rid(*i) == *id))
    );
}

/// Kill a learner mid-catch-up: no voter-set change lands until a healthy
/// learner catches up, then the healthy spare completes the replacement.
#[tokio::test]
async fn dead_learner_mid_catch_up_does_not_block_replacement() {
    if !cluster_tests_enabled() {
        eprintln!("skipping cluster test (set RELIABURGER_CLUSTER_TESTS=1)");
        return;
    }

    let harness = Harness::start().await;
    let leader = harness.wait_for_leader().await;
    let initial: BTreeSet<u64> = (0..3).map(rid).collect();

    // Cut node-4 (the preferred spare) off from the voters at the Raft
    // layer while gossip still sees it alive: it can be added as a learner
    // but can never catch up.
    for voter in 0..3 {
        harness.router.partition(rid(3), rid(voter)).await;
    }

    // Kill a non-leader voter.
    let victim = if leader == 2 { 1 } else { 2 };
    harness.nodes[victim].shutdown().await.unwrap();
    let alive: Vec<usize> = (0..5).filter(|i| *i != victim).collect();
    harness.set_gossip(&alive);

    // The lagging learner appears in the membership...
    let learner_added = wait_until(Duration::from_secs(10), || {
        harness.membership_nodes_of(leader).contains(&rid(3))
    })
    .await;
    assert!(learner_added, "replacement learner never added");

    // ...but the voter set must not change while it cannot catch up.
    let start = tokio::time::Instant::now();
    while tokio::time::Instant::now() < start + Duration::from_secs(3) {
        assert_eq!(
            harness.voters_of(leader),
            initial,
            "voter set changed with only a lagging learner available"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Now the lagging learner dies (drops out of gossip too). The healthy
    // spare node-5 takes over the replacement.
    let alive: Vec<usize> = alive.into_iter().filter(|i| *i != 3).collect();
    harness.set_gossip(&alive);

    let survivor = if victim == 2 { 1 } else { 2 };
    let expected: BTreeSet<u64> = BTreeSet::from([rid(0), rid(survivor), rid(4)]);
    let leader_id = rid(leader);
    let healed = wait_until(Duration::from_secs(15), || {
        let voters = harness.voters_of(leader);
        assert!(!voters.contains(&rid(3)), "lagging learner was promoted");
        assert!(voters.contains(&leader_id), "leader removed mid-change");
        voters == expected
    })
    .await;
    assert!(
        healed,
        "council did not heal to {expected:?}; got {:?}",
        harness.voters_of(leader)
    );

    // Both the dead voter and the dead learner are evicted entirely.
    let cleaned = wait_until(Duration::from_secs(5), || {
        let nodes = harness.membership_nodes_of(leader);
        !nodes.contains(&rid(victim)) && !nodes.contains(&rid(3))
    })
    .await;
    assert!(cleaned, "dead voter or dead learner still in membership");
}

/// A node flapping inside the hysteresis window causes no membership churn:
/// no eviction, no learner added, voter set untouched.
#[tokio::test]
async fn flapping_node_inside_window_causes_no_churn() {
    if !cluster_tests_enabled() {
        eprintln!("skipping cluster test (set RELIABURGER_CLUSTER_TESTS=1)");
        return;
    }

    let harness = Harness::start().await;
    let leader = harness.wait_for_leader().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let initial: BTreeSet<u64> = (0..3).map(rid).collect();
    assert_eq!(harness.voters_of(leader), initial);
    assert_eq!(harness.membership_nodes_of(leader), initial);

    // Flap a non-leader voter: gone from gossip for 150ms at a time, well
    // inside the 600ms dead window. Its Raft stays up — this is a gossip
    // blip, not a real death.
    let flapper = if leader == 2 { 1 } else { 2 };
    let without: Vec<usize> = (0..5).filter(|i| *i != flapper).collect();
    for _ in 0..6 {
        harness.set_gossip(&without);
        tokio::time::sleep(Duration::from_millis(150)).await;
        harness.set_gossip(&[0, 1, 2, 3, 4]);
        tokio::time::sleep(Duration::from_millis(150)).await;

        // No churn at any point: same voters, no learners added.
        assert_eq!(harness.voters_of(leader), initial, "voter churn on flap");
        assert_eq!(
            harness.membership_nodes_of(leader),
            initial,
            "learner added for a flapping voter"
        );
    }
}
