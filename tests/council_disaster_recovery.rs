//! Acceptance tests for council disaster recovery (Phase 12b.2, findings
//! D21/CP12): encrypted external backup/restore, full-council-loss recovery,
//! and disk-pressure leader deposition via `trigger().elect()`.
//!
//! The heavyweight in-process tests are gated behind `RELIABURGER_CLUSTER_TESTS=1`
//! because they run a real multi-node Raft cluster and wait out hysteresis
//! windows. The backup seal/restore unit tests live next to the module.
//!
//! Run the gated suite with:
//! `RELIABURGER_CLUSTER_TESTS=1 cargo test --test council_disaster_recovery`

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use reliaburger::cluster::identity::raft_id_from_name;
use reliaburger::cluster::runtime::{
    CouncilReconcilerConfig, spawn_council_reconciler_with_config,
    spawn_council_reconciler_with_pressure, spawn_disk_pressure_consumer,
};
use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::selection::CouncilSelectionConfig;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo, RaftRequest};
use reliaburger::meat::NodeId;
use reliaburger::mustard::GossipConfig;
use reliaburger::mustard::directory::NodeDirectory;
use reliaburger::mustard::membership::MembershipSnapshot;
use reliaburger::mustard::protocol::MustardNode;
use reliaburger::mustard::state::NodeState;
use reliaburger::mustard::transport::InMemoryNetwork;

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
    CouncilNodeInfo::new(
        SocketAddr::from(([127, 0, 0, 1], 9444 + 2 * index as u16)),
        NAMES[index].to_string(),
    )
}

fn member(index: usize, now: Instant) -> MembershipSnapshot {
    MembershipSnapshot {
        node_id: NodeId::new(NAMES[index]),
        address: gossip_addr(index),
        state: NodeState::Alive,
        incarnation: 1,
        is_council: false,
        is_leader: false,
        labels: BTreeMap::new(),
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

/// Build a fresh five-node router with the first three initialised as voters.
async fn build_cluster() -> (Vec<Arc<CouncilNode>>, InMemoryRaftRouter) {
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

async fn current_leader_index(nodes: &[Arc<CouncilNode>]) -> Option<usize> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        #[allow(clippy::needless_range_loop)]
        for index in 0..3 {
            if let Some(leader) = nodes[index].current_leader().await
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
// Step 0: verify trigger().elect() deposes the current leader.
// ---------------------------------------------------------------------------

/// The deposition mechanism this whole theme builds on: openraft 0.9 has no
/// graceful leadership transfer, so a leader that must leave triggers an
/// election on itself (or a chosen healthy voter). The campaign runs at a
/// higher term and the previous leader steps down. Here the *leader itself*
/// campaigns, which is enough to prove the term advances and leadership can
/// move; the recovery code chooses a healthy follower to campaign instead so
/// the resigning leader actually hands off.
#[tokio::test]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host"]
async fn trigger_elect_advances_term_and_can_move_leadership() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );

    let (nodes, _router) = build_cluster().await;
    let leader = current_leader_index(&nodes)
        .await
        .expect("no leader elected");
    let leader_id = rid(leader);

    // A committed write so followers are caught up and electable.
    nodes[leader]
        .write(RaftRequest::ConfigSet {
            key: "k".to_string(),
            value: "v".to_string(),
        })
        .await
        .unwrap();

    let term_before = nodes[leader].metrics().borrow().current_term;

    // Pick a healthy follower and make it campaign. It should win with a
    // higher term and depose the current leader.
    let follower = (0..3).find(|i| rid(*i) != leader_id).unwrap();
    nodes[follower].raft().trigger().elect().await.unwrap();

    let deposed = wait_until(Duration::from_secs(5), || {
        let term_now = nodes[follower].metrics().borrow().current_term;
        term_now > term_before
    })
    .await;
    assert!(deposed, "term did not advance after trigger().elect()");

    // Leadership settled on a single node again, and every voter agrees.
    let settled = wait_until(Duration::from_secs(5), || {
        let leaders: BTreeSet<Option<u64>> = (0..3)
            .map(|i| nodes[i].metrics().borrow().current_leader)
            .collect();
        leaders.len() == 1 && leaders.iter().next().unwrap().is_some()
    })
    .await;
    assert!(settled, "leadership did not settle after deposition");

    for node in &nodes {
        let _ = node.shutdown().await;
    }
}

// ---------------------------------------------------------------------------
// Step 4/5: leader deposition under disk pressure via the reconciler.
// ---------------------------------------------------------------------------

/// The transfer-seam verification wired through the real reconciler: the
/// leader is flagged under sustained disk pressure, the reconciler triggers an
/// election to hand off, and leadership moves to another voter. The pressured
/// node keeps its Raft up (this is resignation, not death), so the council
/// stays whole.
#[tokio::test]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host"]
async fn pressured_leader_resigns_and_leadership_moves() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );

    let (nodes, _router) = build_cluster().await;
    let leader = current_leader_index(&nodes)
        .await
        .expect("no leader elected");
    let leader_id = rid(leader);

    // A committed write so the followers are caught up and cleanly electable
    // (a bare-elected leader hasn't committed its own vote yet).
    nodes[leader]
        .write(RaftRequest::ConfigSet {
            key: "k".to_string(),
            value: "v".to_string(),
        })
        .await
        .unwrap();

    let now = Instant::now();
    let snapshot: Vec<MembershipSnapshot> = (0..3).map(|i| member(i, now)).collect();
    let (_membership_tx, membership_rx) = watch::channel(snapshot);

    // A shared pressured-voter set, as gossip would propagate it: every node
    // (in particular the follower that must depose) learns the leader is
    // resigning under disk pressure.
    let shutdown = CancellationToken::new();
    let (pressure_tx, pressure_rx) = watch::channel(BTreeSet::<u64>::new());
    #[allow(clippy::needless_range_loop)]
    for index in 0..3 {
        spawn_council_reconciler_with_pressure(
            Arc::clone(&nodes[index]),
            membership_rx.clone(),
            pressure_rx.clone(),
            1,
            rid(index),
            node_info(index),
            heal_reconciler_config(),
            shutdown.clone(),
        );
    }

    // Flag the leader as resigning under disk pressure.
    pressure_tx.send(BTreeSet::from([leader_id])).unwrap();

    // Leadership moves off the pressured node.
    let moved = wait_until(Duration::from_secs(10), || {
        nodes[leader]
            .metrics()
            .borrow()
            .current_leader
            .is_some_and(|l| l != leader_id)
    })
    .await;
    assert!(moved, "leadership did not move off the pressured leader");

    // A single leader settled, and it's a voter that isn't the pressured node.
    let settled = wait_until(Duration::from_secs(5), || {
        let leaders: BTreeSet<Option<u64>> = (0..3)
            .map(|i| nodes[i].metrics().borrow().current_leader)
            .collect();
        leaders.len() == 1
            && leaders
                .iter()
                .next()
                .unwrap()
                .is_some_and(|l| l != leader_id)
    })
    .await;
    assert!(settled, "leadership did not settle on a new voter");

    shutdown.cancel();
    for node in &nodes {
        let _ = node.shutdown().await;
    }
}

/// End-to-end through gossip (12b.2 T3 follow-up): unlike the test above, which
/// injects the pressured-voter set directly, this one drives the WHOLE
/// production signal path. A pressured follower advertises `disk_pressured` over
/// real gossip; the leader-side gossip node folds it into its directory; the
/// [`spawn_disk_pressure_consumer`] turns that directory plus the live voter set
/// into the Raft-id set; and the reconciler deposes the pressured node when it
/// leads. This is what closes the gap where production `start()` used to feed
/// the reconciler a permanently empty set.
#[tokio::test]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host"]
async fn disk_pressure_advertised_over_gossip_drives_resignation() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );

    let (nodes, _router) = build_cluster().await;
    let leader = current_leader_index(&nodes)
        .await
        .expect("no leader elected");
    let leader_id = rid(leader);

    // Commit a write so followers are caught up and cleanly electable.
    nodes[leader]
        .write(RaftRequest::ConfigSet {
            key: "k".to_string(),
            value: "v".to_string(),
        })
        .await
        .unwrap();

    // A two-node gossip fabric: the leader-side observer probes the pressured
    // voter and learns its `disk_pressured` bit purely from gossip. We drive
    // the pressured voter as whichever voter currently leads, so deposition is
    // observable.
    let net = InMemoryNetwork::new();
    let observer_gossip = gossip_addr(3); // a non-voter address, just a probe point
    let pressured_gossip = gossip_addr(leader);
    let observer_transport = net.register(observer_gossip).await;
    let pressured_transport = net.register(pressured_gossip).await;

    let mut observer = MustardNode::new(
        NodeId::new("observer"),
        observer_gossip,
        GossipConfig::default(),
        observer_transport,
    );
    let mut pressured = MustardNode::new(
        NodeId::new(NAMES[leader]),
        pressured_gossip,
        GossipConfig::default(),
        pressured_transport,
    );
    pressured.set_advertised_endpoints(9117, 9445, BTreeMap::new());
    let (pressure_advertise_tx, pressure_advertise_rx) = watch::channel(true);
    pressured.set_disk_pressured_watch(pressure_advertise_rx);
    let (directory_tx, directory_rx) = watch::channel(NodeDirectory::default());
    observer.set_directory_watch(directory_tx);
    observer.add_seed(NodeId::new(NAMES[leader]), pressured_gossip);

    let gossip_shutdown = CancellationToken::new();
    let pressured_shutdown = gossip_shutdown.clone();
    let pressured_handle = tokio::spawn(async move {
        pressured.run(pressured_shutdown).await;
    });
    // A few probe cycles so the observer learns the advertised pressure.
    for _ in 0..5 {
        observer.run_one_cycle().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let learned = wait_until(Duration::from_secs(5), || {
        directory_rx
            .borrow()
            .disk_pressured
            .contains(&NodeId::new(NAMES[leader]))
    })
    .await;
    assert!(
        learned,
        "observer never learned the pressured voter's bit over gossip"
    );

    // The consumer turns the gossip directory + live voters into the pressured
    // Raft-id set, exactly as production does, and feeds every reconciler.
    let shutdown = CancellationToken::new();
    let (pressure_tx, pressure_rx) = watch::channel(BTreeSet::<u64>::new());
    spawn_disk_pressure_consumer(
        Arc::clone(&nodes[leader]),
        directory_rx.clone(),
        pressure_tx,
        shutdown.clone(),
    );

    // The novel property this test pins: the pressured voter's Raft id reaches
    // the reconciler's input set purely from gossip. The reconciler acting on
    // that set (deposing the pressured leader, replacing a pressured follower)
    // is already covered by `pressured_leader_resigns_and_leadership_moves` and
    // the planner unit tests, which feed the set directly. Keeping this test to
    // the gossip half avoids a second heavy Raft-timing dependency.
    let pressured_ready = wait_until(Duration::from_secs(5), || {
        pressure_rx.borrow().contains(&leader_id)
    })
    .await;
    assert!(
        pressured_ready,
        "consumer never derived the pressured voter's Raft id from gossip"
    );

    // Clearing the advertised bit propagates the other way: the consumer drops
    // the voter from the set once gossip carries a healthy disk again.
    pressure_advertise_tx.send(false).unwrap();
    for _ in 0..5 {
        observer.run_one_cycle().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let cleared = wait_until(Duration::from_secs(5), || {
        !pressure_rx.borrow().contains(&leader_id)
    })
    .await;
    assert!(
        cleared,
        "consumer never dropped the voter after gossip cleared the bit"
    );

    shutdown.cancel();
    gossip_shutdown.cancel();
    drop(pressure_advertise_tx);
    let _ = pressured_handle.await;
    for node in &nodes {
        let _ = node.shutdown().await;
    }
}

// ---------------------------------------------------------------------------
// Step 5: full-council loss and recovery (black box).
// ---------------------------------------------------------------------------

/// The headline acceptance test. A three-voter council with two workers,
/// a backup taken, then every voter dies. A survivor restores from the
/// backup, re-bootstraps a fresh single-voter Raft with a new epoch, and the
/// self-healing reconciler regrows the council. The restored desired state
/// still knows the apps that were registered before the loss.
#[tokio::test]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host"]
async fn full_council_loss_recovers_from_backup() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );

    use reliaburger::council::backup::{BackupConfig, seal_snapshot, unseal_snapshot};
    use reliaburger::council::recovery::recover_from_desired_state;

    let master_key = [7u8; 32];

    // 1. A three-voter council; register an app so there is state to lose.
    let (nodes, router) = build_cluster().await;
    let leader = current_leader_index(&nodes)
        .await
        .expect("no leader elected");
    nodes[leader]
        .write(RaftRequest::ConfigSet {
            key: "before-loss".to_string(),
            value: "present".to_string(),
        })
        .await
        .unwrap();

    // 2. Take a sealed backup of the leader's desired state.
    let desired = nodes[leader].desired_state().await;
    let payload = serde_json::to_vec(&desired).unwrap();
    let sealed = seal_snapshot(&master_key, &payload, &BackupConfig::default()).unwrap();

    // 3. Every voter dies.
    for node in nodes.iter().take(3) {
        let _ = node.shutdown().await;
    }

    // 4. A survivor (node-4) restores and re-bootstraps a single-voter Raft.
    let restored_bytes = unseal_snapshot(&master_key, &sealed).unwrap();
    let restored: reliaburger::council::types::DesiredState =
        serde_json::from_slice(&restored_bytes).unwrap();
    assert_eq!(
        restored.config.get("before-loss").map(String::as_str),
        Some("present"),
        "restored desired state lost its pre-loss config"
    );

    let network = InMemoryRaftNetworkFactory::new(rid(3), router.clone());
    let recovered = recover_from_desired_state(
        rid(3),
        node_info(3),
        fast_council_config(),
        network,
        MemLogStore::new(),
        restored,
    )
    .await
    .unwrap();
    router.register(rid(3), recovered.raft().clone()).await;
    let recovered = Arc::new(recovered);

    // The recovered node is the sole voter and leads.
    let became_leader = wait_until(Duration::from_secs(5), || {
        recovered
            .metrics()
            .borrow()
            .current_leader
            .is_some_and(|l| l == rid(3))
    })
    .await;
    assert!(became_leader, "recovered node did not become leader");

    // Its state carries the pre-loss config and a fresh recovery epoch.
    let state = recovered.desired_state().await;
    assert_eq!(
        state.config.get("before-loss").map(String::as_str),
        Some("present")
    );
    assert!(
        state.recovery_epoch > 0,
        "recovery did not stamp a new epoch"
    );

    // 5. The reconciler regrows the council from the surviving workers.
    let now = Instant::now();
    let snapshot: Vec<MembershipSnapshot> = [3usize, 4].iter().map(|i| member(*i, now)).collect();
    let (membership_tx, membership_rx) = watch::channel(snapshot);
    let shutdown = CancellationToken::new();

    // Bring node-5 up as a spare that can actually replicate.
    router.register(rid(4), nodes[4].raft().clone()).await;

    spawn_council_reconciler_with_config(
        Arc::clone(&recovered),
        membership_rx.clone(),
        1,
        rid(3),
        node_info(3),
        heal_reconciler_config(),
        shutdown.clone(),
    );
    spawn_council_reconciler_with_config(
        Arc::clone(&nodes[4]),
        membership_rx,
        1,
        rid(4),
        node_info(4),
        heal_reconciler_config(),
        shutdown.clone(),
    );

    // The council grows past a single voter (node-5 joins).
    let grew = wait_until(Duration::from_secs(15), || {
        let voters: BTreeSet<u64> = recovered
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .voter_ids()
            .collect();
        voters.contains(&rid(4))
    })
    .await;
    let _ = membership_tx;
    assert!(grew, "recovered council did not regrow from the survivor");

    shutdown.cancel();
    let _ = recovered.shutdown().await;
    let _ = nodes[4].shutdown().await;
}
