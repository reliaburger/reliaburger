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
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use reliaburger::cluster::runtime::{
    spawn_council_reconciler_with_config, spawn_council_reconciler_with_pressure,
    spawn_disk_pressure_consumer,
};
use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::InMemoryRaftNetworkFactory;
use reliaburger::council::types::RaftRequest;
use reliaburger::meat::NodeId;
use reliaburger::mustard::GossipConfig;
use reliaburger::mustard::directory::NodeDirectory;
use reliaburger::mustard::membership::MembershipSnapshot;
use reliaburger::mustard::protocol::MustardNode;
use reliaburger::mustard::transport::InMemoryNetwork;

#[path = "support/bun_process.rs"]
mod bun_process;
#[path = "support/cluster.rs"]
mod cluster_support;
use bun_process::{PortBlockReservation, reserve_port_block_lease};
use cluster_support::{
    NAMES, build_council, cluster_tests_enabled, fast_council_config, gossip_addr,
    heal_reconciler_config, initial_voter_leader_index, member, node_info, rid,
};

/// How often `wait_until` re-checks its condition in this binary.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

async fn wait_until(timeout: Duration, cond: impl FnMut() -> bool) -> bool {
    cluster_support::wait_until(timeout, POLL_INTERVAL, cond).await
}

// ---------------------------------------------------------------------------
// Step 0: verify trigger().elect() deposes the current leader.
// ---------------------------------------------------------------------------

/// The deposition mechanism this whole theme builds on: openraft 0.9 has no
/// graceful leadership transfer, so a follower's campaign at a higher term
/// deposes the leader. This shows only that the term advances and a single
/// leader settles; that leader may be the old one, because the election
/// after a deposition favours the longest log. Moving leadership needs the
/// old leader to hold its elections too, which
/// `leadership_moves_off_the_leader_every_time_under_write_load` shows (#755).
#[tokio::test]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn trigger_elect_advances_the_term_and_leadership_settles() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );

    let (nodes, _router) = build_council().await;
    let leader = initial_voter_leader_index(&nodes)
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

    // Pick a healthy follower and make it campaign. Its higher term deposes
    // the current leader; who wins the next election is not decided here.
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
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn pressured_leader_resigns_and_leadership_moves() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );

    let (nodes, _router) = build_council().await;
    let leader = initial_voter_leader_index(&nodes)
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

    // Writes in flight while the follower campaigns, as in #646: the leader
    // often holds an entry the candidate lacks, so the candidate loses and
    // the old leader would win the next election if it could campaign.
    let writing = CancellationToken::new();
    let writer = spawn_writer(&nodes[..3], writing.clone());
    let term_before = nodes[leader].metrics().borrow().current_term;

    // Flag the leader as resigning under disk pressure.
    pressure_tx.send(BTreeSet::from([leader_id])).unwrap();

    // Leadership moves off the pressured node, and the pressured node never
    // wins it back in the meantime: a moved term alone isn't a handover.
    let moved = wait_until(Duration::from_secs(10), || {
        assert!(
            leading_term(&nodes, leader_id).is_none_or(|term| term <= term_before),
            "the pressured leader won leadership back"
        );
        nodes[leader]
            .metrics()
            .borrow()
            .current_leader
            .is_some_and(|l| l != leader_id)
    })
    .await;
    writing.cancel();
    let _ = writer.await;
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

/// The highest term at which `id` was seen leading, sampled from `nodes`.
fn leading_term(nodes: &[Arc<reliaburger::council::CouncilNode>], id: u64) -> Option<u64> {
    nodes
        .iter()
        .map(|node| node.metrics().borrow().clone())
        .filter(|metrics| metrics.current_leader == Some(id))
        .map(|metrics| metrics.current_term)
        .max()
}

/// Keep writing to whichever voter leads until `stop`, so elections happen
/// while the leader holds entries its followers haven't received yet.
fn spawn_writer(
    nodes: &[Arc<reliaburger::council::CouncilNode>],
    stop: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let nodes: Vec<_> = nodes.to_vec();
    tokio::spawn(async move {
        let mut sequence = 0u64;
        while !stop.is_cancelled() {
            for node in &nodes {
                sequence += 1;
                let _ = tokio::time::timeout(
                    Duration::from_millis(200),
                    node.write(RaftRequest::ConfigSet {
                        key: "load".to_string(),
                        value: sequence.to_string(),
                    }),
                )
                .await;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
}

/// The handover `POST /v1/cluster/elect` and disk-pressure deposition rely
/// on (#755): the leader holds its elections and another voter campaigns
/// until it leads. Five handovers in a row, under constant writes, must each
/// move leadership, and no old leader may win it back on the way.
#[tokio::test]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn leadership_moves_off_the_leader_every_time_under_write_load() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );

    let (nodes, _router) = build_council().await;
    let voters = &nodes[..3];
    let mut leader = initial_voter_leader_index(&nodes)
        .await
        .expect("no leader elected");
    let writing = CancellationToken::new();
    let writer = spawn_writer(voters, writing.clone());

    for round in 0..5 {
        let leader_id = rid(leader);
        let term_before = voters[leader].metrics().borrow().current_term;
        // The voter taking over is one of the others; which one wins is
        // Raft's choice by log length, and either is a handover.
        let candidate = (0..3).find(|index| *index != leader).unwrap();
        voters[leader].hold_elections_until(Instant::now() + Duration::from_secs(30));
        let new_leader = voters[candidate]
            .campaign_until_leadership_moves(
                Some(leader_id),
                Instant::now() + Duration::from_secs(20),
            )
            .await
            .unwrap_or_else(|error| panic!("round {round}: {error}"));
        assert_ne!(
            new_leader, leader_id,
            "round {round}: leadership did not move"
        );
        assert!(
            leading_term(voters, leader_id).is_none_or(|term| term <= term_before),
            "round {round}: the old leader won leadership back"
        );
        let released =
            wait_until(Duration::from_secs(5), || !voters[leader].elections_held()).await;
        assert!(
            released,
            "round {round}: the old leader's hold outlived the handover"
        );
        leader = (0..3).find(|index| rid(*index) == new_leader).unwrap();
    }

    writing.cancel();
    let _ = writer.await;
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
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn disk_pressure_advertised_over_gossip_drives_resignation() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );

    let (nodes, _router) = build_council().await;
    let leader = initial_voter_leader_index(&nodes)
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
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn full_council_loss_recovers_from_backup() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );

    use reliaburger::council::backup::{BackupConfig, seal_snapshot, unseal_snapshot};
    use reliaburger::council::recovery::recover_from_desired_state;

    let master_key = [7u8; 32];

    // 1. A three-voter council; register an app so there is state to lose.
    let (nodes, router) = build_council().await;
    let leader = initial_voter_leader_index(&nodes)
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

    // Two live nodes keep one voter (D12, #694): node-5 never becomes a
    // second voter that quorum would then depend on. Regrowth onto three
    // nodes is `recovered_council_regrows_onto_a_fresh_node`.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        let voters: BTreeSet<u64> = recovered
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .voter_ids()
            .collect();
        assert_eq!(
            voters,
            BTreeSet::from([rid(3)]),
            "two live nodes grew an even council"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    let _ = membership_tx;
    recovered
        .write(RaftRequest::ConfigSet {
            key: "after-recovery".to_string(),
            value: "present".to_string(),
        })
        .await
        .expect("the recovered single voter stopped committing");

    shutdown.cancel();
    let _ = recovered.shutdown().await;
    let _ = nodes[4].shutdown().await;
}

// ---------------------------------------------------------------------------
// Recovery-epoch fence: old voters returning after `council recover --force`.
// ---------------------------------------------------------------------------

/// Ports per fence node: gossip, +1 Raft, +2 reporting, +3 API.
const FENCE_PORTS_PER_NODE: u16 = 4;

/// Node slots across the fence tests: 0 to 2 for the split-brain test, 3
/// to 5 for the regrowth test, 6 to 8 for the two-survivor test and 9 to
/// 11 for the slow-gossip test, so their data directories stay apart when
/// `cargo test` runs them in one process.
const FENCE_SLOTS: u16 = 12;

/// A fence test's leased ports, one four-port block per node slot. The test
/// holds it until it ends, so no other fixture on the host binds a stopped
/// node's ports before the node comes back on them (#744).
struct FencePorts(PortBlockReservation);

impl FencePorts {
    fn lease() -> Self {
        Self(reserve_port_block_lease(FENCE_PORTS_PER_NODE * FENCE_SLOTS))
    }

    fn gossip(&self, index: usize) -> std::net::SocketAddr {
        std::net::SocketAddr::from((
            [127, 0, 0, 1],
            self.0.base + (index as u16) * FENCE_PORTS_PER_NODE,
        ))
    }
}

fn fence_data_dir(index: usize) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("rb-recovery-fence-{}-{index}", std::process::id()))
}

/// One running node of the split-brain test: the real cluster runtime, as
/// `bun --cluster` starts it, on a data directory that survives restarts.
struct FenceNode {
    council: Arc<reliaburger::council::node::CouncilNode>,
    shutdown: CancellationToken,
    _handle: reliaburger::bun::agent::ClusterHandle,
    _runtime: reliaburger::cluster::runtime::ClusterRuntime,
}

impl FenceNode {
    /// Start node `index` on its persistent data directory. A restarted node
    /// may find its previous incarnation's redb files still locked for a
    /// moment while the old tasks wind down, so retry for a few seconds.
    async fn start(ports: &FencePorts, index: usize, seeds: Vec<std::net::SocketAddr>) -> Self {
        use reliaburger::cluster::runtime::{ClusterParams, start};
        let gossip = ports.gossip(index);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let shutdown = CancellationToken::new();
            let params = ClusterParams {
                node_name: format!("fence{index}"),
                gossip_addr: gossip,
                raft_port: gossip.port() + 1,
                reporting_port: gossip.port() + 2,
                api_port: gossip.port() + 3,
                reporting_config: Default::default(),
                seeds: seeds.clone(),
                wrapping_ikm: None,
                bootstrap_security_state: None,
                bootstrap_council_size: None,
                data_dir: fence_data_dir(index),
                mayo: None,
                rollup_interval: Duration::from_secs(60),
                identity: None,
                backup: Default::default(),
                labels: Default::default(),
                self_disk_pressured_rx: None,
                job_capabilities_rx: None,
                readiness: None,
                recovery_probe_window: None,
                council_size_cap: None,
            };
            match start(params, shutdown.clone()).await {
                Ok((mut handle, runtime)) => {
                    let council = handle.council.clone().expect("cluster mode has a council");
                    // Nothing answers the report worker's snapshot requests in
                    // this harness; drain them so its channel never fills.
                    let (_unused_tx, unused_rx) = tokio::sync::mpsc::channel(1);
                    let mut snapshot_rx = std::mem::replace(&mut handle.snapshot_rx, unused_rx);
                    tokio::spawn(async move { while snapshot_rx.recv().await.is_some() {} });
                    return Self {
                        council,
                        shutdown,
                        _handle: handle,
                        _runtime: runtime,
                    };
                }
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "node {index} did not start: {error}"
                    );
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
    }

    fn voters(&self) -> BTreeSet<u64> {
        self.council
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .voter_ids()
            .collect()
    }

    async fn stop(self) {
        self.shutdown.cancel();
        let _ = self.council.shutdown().await;
        drop(self);
        // Let the cancelled tasks release the redb files and sockets.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Write `key` through whichever of `nodes` will take it, within `timeout`.
/// Returns the index of the node that committed it, if any did.
async fn commit_on_any(nodes: &[&FenceNode], key: &str, timeout: Duration) -> Option<usize> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        for (index, node) in nodes.iter().enumerate() {
            let write = node.council.write(RaftRequest::ConfigSet {
                key: key.to_string(),
                value: "written".to_string(),
            });
            if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_secs(2), write).await {
                return Some(index);
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    None
}

/// The external report (split brain after `council recover --force`): two of
/// three voters stop, the operator recovers the survivor, and later the two
/// stopped voters come back. They still hold the old three-voter membership,
/// so between them they are a majority of it. The recovery-epoch fence must
/// keep them from forming or serving that quorum.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn old_voters_returning_after_recovery_cannot_form_a_quorum() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );
    let ports = FencePorts::lease();
    for index in 0..3 {
        let _ = std::fs::remove_dir_all(fence_data_dir(index));
        // Bun stamps the state format before it starts the runtime.
        reliaburger::compatibility::ensure_state_compatible(&fence_data_dir(index)).unwrap();
    }

    // 1. A three-voter council on real transports and durable storage.
    let n0 = FenceNode::start(&ports, 0, Vec::new()).await;
    let n1 = FenceNode::start(&ports, 1, vec![ports.gossip(0)]).await;
    let n2 = FenceNode::start(&ports, 2, vec![ports.gossip(0)]).await;
    let all_ids: BTreeSet<u64> = (0..3)
        .map(|i| reliaburger::cluster::identity::raft_id_from_name(&format!("fence{i}")))
        .collect();
    let formed = wait_until(Duration::from_secs(60), || {
        [&n0, &n1, &n2].iter().all(|n| n.voters() == all_ids)
    })
    .await;
    assert!(formed, "the council never grew to three voters");
    assert!(
        commit_on_any(&[&n0, &n1, &n2], "before-loss", Duration::from_secs(20))
            .await
            .is_some(),
        "the healthy council never committed a write"
    );
    let backup = n0.council.desired_state().await;

    // 2. Two voters stop: the majority is lost. The operator stops the
    //    survivor too and runs `council recover --force` against it offline.
    n1.stop().await;
    n2.stop().await;
    n0.stop().await;
    reliaburger::council::recovery::recover_data_dir(&fence_data_dir(0), backup, false).unwrap();

    // 3. The recovered survivor comes back alone on a new epoch and commits.
    let recovered = FenceNode::start(&ports, 0, Vec::new()).await;
    assert!(
        commit_on_any(&[&recovered], "after-recovery", Duration::from_secs(20))
            .await
            .is_some(),
        "the recovered council never committed a write"
    );
    assert_eq!(recovered.council.desired_state().await.recovery_epoch, 1);

    // 4. The two old voters return, with their pre-recovery data and seeds.
    let old1 = FenceNode::start(&ports, 1, vec![ports.gossip(0)]).await;
    let old2 = FenceNode::start(&ports, 2, vec![ports.gossip(0)]).await;

    // Between them they are two of the old membership's three voters, a
    // majority. They must never commit on it.
    let split = commit_on_any(&[&old1, &old2], "split-brain", Duration::from_secs(20)).await;
    assert_eq!(
        split, None,
        "an old-epoch voter committed a write after recovery: the old council formed a quorum"
    );

    // Both know they are fenced, and neither claims a leader any more.
    let fenced = wait_until(Duration::from_secs(20), || {
        [&old1, &old2].iter().all(|node| {
            node.council
                .recovery_fence()
                .is_some_and(|fence| fence.snapshot().fenced_by() == Some(1))
                && node.council.metrics().borrow().current_leader.is_none()
        })
    })
    .await;
    assert!(fenced, "the old voters never learned they were fenced");
    let refused = old1
        .council
        .write(RaftRequest::ConfigSet {
            key: "refused".to_string(),
            value: "never".to_string(),
        })
        .await;
    assert!(
        matches!(
            refused,
            Err(reliaburger::council::CouncilError::Fenced {
                epoch: 0,
                newer_epoch: 1
            })
        ),
        "a fenced voter must refuse writes with the fence error, got {refused:?}"
    );

    // Every node's own council status, compared the way `relish council
    // status`, the `relish status` header and `relish wtf` compare them,
    // shows the recovered council healthy and both old voters fenced.
    let mut observations = Vec::new();
    for (name, node) in [("fence0", &recovered), ("fence1", &old1), ("fence2", &old2)] {
        let metrics = node.council.metrics().borrow().clone();
        let status = reliaburger::bun::agent::council_status(&node.council, &metrics).await;
        observations.push(
            reliaburger::relish::council_view::CouncilNodeObservation::answered(name, &status),
        );
    }
    let summary = reliaburger::relish::council_view::summarise(&observations);
    assert_eq!(summary.epoch, Some(1));
    assert_eq!(summary.leaders, vec!["fence0".to_string()]);
    assert_eq!(summary.voters, vec!["fence0".to_string()]);
    assert!(summary.quorum_ok());
    let fenced: Vec<&str> = summary.fenced.iter().map(|n| n.node_id.as_str()).collect();
    assert_eq!(fenced, vec!["fence1", "fence2"]);
    assert_eq!(
        summary.epochs.len(),
        1,
        "fenced voters must not count as a council"
    );
    let table = reliaburger::relish::council_view::render_council_status(&observations, &summary);
    assert!(table.contains("fence1  fenced"), "council status:\n{table}");
    let header = reliaburger::relish::council_view::render_status_header(&summary);
    assert!(
        header.contains("warning: fenced: fence1"),
        "status header:\n{header}"
    );

    // The fence survives a restart: the old voter comes back still fenced.
    old2.stop().await;
    let old2 = FenceNode::start(&ports, 2, vec![ports.gossip(0)]).await;
    assert_eq!(
        old2.council
            .recovery_fence()
            .and_then(|fence| fence.snapshot().fenced_by()),
        Some(1),
        "a restarted old voter forgot its fence"
    );

    // The recovered council is unaffected and still commits.
    assert!(
        commit_on_any(&[&recovered], "still-serving", Duration::from_secs(10))
            .await
            .is_some(),
        "the recovered council stopped committing once the old voters returned"
    );

    old1.stop().await;
    old2.stop().await;
    recovered.stop().await;
    for index in 0..3 {
        let _ = std::fs::remove_dir_all(fence_data_dir(index));
    }
}

/// After recovery the council must be able to regrow onto nodes with a
/// fresh data directory: that is both how the reconciler replaces the lost
/// voters and how an operator re-enrols a wiped old voter. A fresh node
/// starts with no recovery epoch of its own, so it must accept the recovered
/// council's RPCs and adopt its epoch. Two fresh nodes join, because two
/// live nodes keep one voter (D12): the council grows from one voter to
/// three in a single change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn recovered_council_regrows_onto_a_fresh_node() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );
    // Node slots 3 to 5, so this test's data directories stay clear of
    // the test above's.
    let ports = FencePorts::lease();
    for index in 3..6 {
        let _ = std::fs::remove_dir_all(fence_data_dir(index));
        reliaburger::compatibility::ensure_state_compatible(&fence_data_dir(index)).unwrap();
    }

    // A single-voter council, recovered offline from its own state.
    let first = FenceNode::start(&ports, 3, Vec::new()).await;
    assert!(
        commit_on_any(&[&first], "before-loss", Duration::from_secs(20))
            .await
            .is_some(),
        "the council never committed a write"
    );
    let backup = first.council.desired_state().await;
    first.stop().await;
    reliaburger::council::recovery::recover_data_dir(&fence_data_dir(3), backup, false).unwrap();
    // Its config still lists a join seed, as most nodes' do: recovery must
    // re-bootstrap it anyway.
    let recovered = FenceNode::start(&ports, 3, vec![ports.gossip(4)]).await;
    assert!(
        commit_on_any(&[&recovered], "after-recovery", Duration::from_secs(20))
            .await
            .is_some(),
        "the recovered council never committed a write"
    );

    // Two fresh nodes join and the reconciler grows the council onto both.
    let fresh = FenceNode::start(&ports, 4, vec![ports.gossip(3)]).await;
    let other = FenceNode::start(&ports, 5, vec![ports.gossip(3)]).await;
    let fresh_id = reliaburger::cluster::identity::raft_id_from_name("fence4");
    let grew = wait_until(Duration::from_secs(60), || {
        recovered.voters().len() == 3
            && recovered.voters().contains(&fresh_id)
            && fresh.voters().contains(&fresh_id)
    })
    .await;
    assert!(
        grew,
        "the recovered council never grew onto a fresh node (voters {:?})",
        recovered.voters()
    );

    // The joined node holds the recovered state, the restored keys included.
    let replicated = wait_until(Duration::from_secs(20), || {
        fresh
            .council
            .recovery_fence()
            .is_some_and(|fence| fence.snapshot().claimed_epoch() == Some(1) && !fence.is_fenced())
    })
    .await;
    assert!(
        replicated,
        "the fresh node never adopted the recovered epoch"
    );
    let state = fresh.council.desired_state().await;
    for key in ["before-loss", "after-recovery"] {
        assert!(
            state.config.contains_key(key),
            "the fresh voter is missing {key:?}: it never received the recovered state"
        );
    }

    fresh.stop().await;
    other.stop().await;
    recovered.stop().await;
    for index in 3..6 {
        let _ = std::fs::remove_dir_all(fence_data_dir(index));
    }
}

/// Form a three-voter council on fence slots `slots`, commit a write, and
/// stop every node, leaving three voters' data directories behind.
async fn form_and_stop_council(ports: &FencePorts, slots: [usize; 3]) {
    for index in slots {
        let _ = std::fs::remove_dir_all(fence_data_dir(index));
        reliaburger::compatibility::ensure_state_compatible(&fence_data_dir(index)).unwrap();
    }
    let first = FenceNode::start(ports, slots[0], Vec::new()).await;
    let second = FenceNode::start(ports, slots[1], vec![ports.gossip(slots[0])]).await;
    let third = FenceNode::start(ports, slots[2], vec![ports.gossip(slots[0])]).await;
    let ids: BTreeSet<u64> = slots
        .iter()
        .map(|i| reliaburger::cluster::identity::raft_id_from_name(&format!("fence{i}")))
        .collect();
    let formed = wait_until(Duration::from_secs(60), || {
        [&first, &second, &third].iter().all(|n| n.voters() == ids)
    })
    .await;
    assert!(formed, "the council never grew to three voters");
    assert!(
        commit_on_any(
            &[&first, &second, &third],
            "before-loss",
            Duration::from_secs(20)
        )
        .await
        .is_some(),
        "the healthy council never committed a write"
    );
    // Let the write reach every voter's log before they stop.
    tokio::time::sleep(Duration::from_secs(1)).await;
    second.stop().await;
    third.stop().await;
    first.stop().await;
}

/// Recover `index` offline from its own data directory, the way `relish
/// council recover` does without `--from`.
async fn recover_from_own_stores(
    index: usize,
    force: bool,
) -> Result<reliaburger::council::types::RecoveryClaim, reliaburger::council::recovery::RecoveryError>
{
    use reliaburger::council::recovery::{RecoverySource, load_recovery_state, recover_data_dir};
    let source = RecoverySource::NodeDataDir(fence_data_dir(index));
    let state = load_recovery_state(&source, None).await?;
    recover_data_dir(&fence_data_dir(index), state, force)
}

/// #694: two survivors recovered independently, each from its own stores,
/// used to land on the same epoch and both accept writes. Now each draws
/// its own recovery id, so when they meet both fence themselves as a split.
/// The operator then recovers one side again: its stores say it is fenced,
/// so that takes `--force`, and it lands above the split epoch. That side
/// serves, and the other stays fenced: exactly one council.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn recovering_two_survivors_independently_fences_one() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );
    // Fence slots 6 to 8, clear of the tests above.
    let ports = FencePorts::lease();
    form_and_stop_council(&ports, [6, 7, 8]).await;

    // 1. Two operators each recover a different survivor from its own
    //    stores. Same epoch, different recovery ids.
    let a_claim = recover_from_own_stores(6, false).await.unwrap();
    let b_claim = recover_from_own_stores(7, false).await.unwrap();
    assert_eq!(a_claim.epoch, b_claim.epoch);
    assert_ne!(
        a_claim.id, b_claim.id,
        "two recoveries shared a recovery id"
    );

    // 2. Both come up and see each other over gossip: a split. Both fence.
    let a = FenceNode::start(&ports, 6, vec![ports.gossip(7)]).await;
    let b = FenceNode::start(&ports, 7, vec![ports.gossip(6)]).await;
    let split = wait_until(Duration::from_secs(30), || {
        [&a, &b].iter().all(|node| {
            node.council
                .recovery_fence()
                .is_some_and(|fence| fence.snapshot().split_with().is_some())
        })
    })
    .await;
    assert!(split, "two councils on one epoch never noticed each other");
    assert_eq!(
        commit_on_any(&[&a, &b], "after-split", Duration::from_secs(5)).await,
        None,
        "a split-fenced council committed a write"
    );
    let mut observations = Vec::new();
    for (name, node) in [("fence6", &a), ("fence7", &b)] {
        let metrics = node.council.metrics().borrow().clone();
        let status = reliaburger::bun::agent::council_status(&node.council, &metrics).await;
        observations.push(
            reliaburger::relish::council_view::CouncilNodeObservation::answered(name, &status),
        );
    }
    let summary = reliaburger::relish::council_view::summarise(&observations);
    assert!(
        summary.warnings()[0].starts_with("split: two recoveries share an epoch"),
        "warnings: {:?}",
        summary.warnings()
    );

    // 3. The operator keeps side A: its stores say it is fenced, so plain
    //    recovery refuses, and --force lands it above the split epoch.
    a.stop().await;
    b.stop().await;
    let refused = recover_from_own_stores(6, false).await;
    assert!(
        matches!(
            refused,
            Err(reliaburger::council::recovery::RecoveryError::FencedSource { .. })
        ),
        "recovering a fenced data directory must need --force, got {refused:?}"
    );
    let kept = recover_from_own_stores(6, true).await.unwrap();
    assert_eq!(kept.epoch, a_claim.epoch + 1);

    let a = FenceNode::start(&ports, 6, vec![ports.gossip(7)]).await;
    let b = FenceNode::start(&ports, 7, vec![ports.gossip(6)]).await;
    assert!(
        commit_on_any(&[&a], "after-resolution", Duration::from_secs(20))
            .await
            .is_some(),
        "the kept council never committed"
    );
    let one_council = wait_until(Duration::from_secs(20), || {
        b.council
            .recovery_fence()
            .is_some_and(|fence| fence.snapshot().fenced_by() == Some(kept.epoch))
    })
    .await;
    assert!(
        one_council,
        "the other side never learned of the newer epoch"
    );
    assert_eq!(
        commit_on_any(&[&b], "other-side", Duration::from_secs(5)).await,
        None,
        "the fenced side committed a write"
    );

    a.stop().await;
    b.stop().await;
    for index in 6..9 {
        let _ = std::fs::remove_dir_all(fence_data_dir(index));
    }
}

/// #694: a restarted old voter used to serve Raft five seconds after its
/// process started, whatever gossip had shown it. Two old voters that came
/// back while the recovered node was still out of reach would then elect a
/// leader on the old epoch and acknowledge writes, and lose them once they
/// heard of the recovery. Now the probe window opens only once gossip has
/// synced and lasts two anti-entropy intervals, unless every persisted peer
/// has advertised sooner: the recovered node, back eight seconds later, is
/// heard before the old voters ever serve.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RELIABURGER_CLUSTER_TESTS=1 and a multi-core host; run with make test-cluster"]
async fn slow_gossip_restart_never_acknowledges_on_the_old_epoch() {
    assert!(
        cluster_tests_enabled(),
        "set RELIABURGER_CLUSTER_TESTS=1 on a provisioned multi-core host"
    );
    // Fence slots 9 to 11, clear of the tests above.
    let ports = FencePorts::lease();
    form_and_stop_council(&ports, [9, 10, 11]).await;
    let backup = reliaburger::council::recovery::load_recovery_state(
        &reliaburger::council::recovery::RecoverySource::NodeDataDir(fence_data_dir(9)),
        None,
    )
    .await
    .unwrap();
    reliaburger::council::recovery::recover_data_dir(&fence_data_dir(9), backup, false).unwrap();

    // The two old voters come back first, with the recovered node still
    // down: they can only hear each other.
    let old1 = FenceNode::start(&ports, 10, Vec::new()).await;
    let old2 = FenceNode::start(&ports, 11, Vec::new()).await;

    // Write against them for longer than the old fixed window, while the
    // recovered node returns eight seconds in. Its join list names an old
    // voter, as a real node's would, so gossip reaches them once it is up.
    let late_start = async {
        tokio::time::sleep(Duration::from_secs(8)).await;
        FenceNode::start(&ports, 9, vec![ports.gossip(10)]).await
    };
    let old = [&old1, &old2];
    let (acknowledged, recovered) = tokio::join!(
        commit_on_any(&old, "old-epoch", Duration::from_secs(20)),
        late_start
    );
    assert_eq!(
        acknowledged, None,
        "an old voter acknowledged a write on the replaced epoch"
    );

    let fenced = wait_until(Duration::from_secs(30), || {
        [&old1, &old2].iter().all(|node| {
            node.council
                .recovery_fence()
                .is_some_and(|fence| fence.snapshot().fenced_by() == Some(1))
        })
    })
    .await;
    assert!(fenced, "the old voters never learned of the recovery");
    assert!(
        commit_on_any(&[&recovered], "recovered", Duration::from_secs(20))
            .await
            .is_some(),
        "the recovered council never committed"
    );

    old1.stop().await;
    old2.stop().await;
    recovered.stop().await;
    for index in 9..12 {
        let _ = std::fs::remove_dir_all(fence_data_dir(index));
    }
}
