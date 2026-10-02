//! A member that joins (or falls behind) after the leader has compacted its
//! log is brought up to date by InstallSnapshot. The snapshot must describe
//! the state it actually holds: if it claims the leader's live position, the
//! member skips every entry applied between the snapshot and the claim, and
//! its state silently diverges (#426).

use std::collections::BTreeMap;
use std::time::Duration;

use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo, RaftRequest};

fn info(id: u64) -> CouncilNodeInfo {
    CouncilNodeInfo {
        addr: format!("127.0.0.1:{}", 9400 + id).parse().unwrap(),
        name: format!("n{id}"),
    }
}

/// Snapshots only when asked, and purges everything a snapshot covers, so
/// any member behind the snapshot must install it.
fn compacting_config() -> CouncilConfig {
    CouncilConfig {
        heartbeat_interval_ms: 50,
        election_timeout_min_ms: 300,
        election_timeout_max_ms: 600,
        snapshot_threshold: 1_000_000,
        max_in_snapshot_log_to_keep: 0,
    }
}

async fn start(id: u64, router: &InMemoryRaftRouter) -> CouncilNode {
    let node = CouncilNode::new(
        id,
        compacting_config(),
        InMemoryRaftNetworkFactory::new(id, router.clone()),
        MemLogStore::new(),
        CouncilStateMachine::new(),
        None,
    )
    .await
    .unwrap();
    router.register(id, node.raft().clone()).await;
    node
}

async fn write(node: &CouncilNode, key: &str) {
    node.write(RaftRequest::ConfigSet {
        key: key.to_string(),
        value: "set".to_string(),
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_learner_installing_a_snapshot_gets_every_entry_applied_after_it() {
    let router = InMemoryRaftRouter::new();
    let leader = start(1, &router).await;
    leader
        .initialize(BTreeMap::from([(1, info(1))]))
        .await
        .unwrap();
    let mut metrics = leader.metrics();
    tokio::time::timeout(
        Duration::from_secs(10),
        metrics.wait_for(|m| m.current_leader == Some(1)),
    )
    .await
    .unwrap()
    .unwrap();

    // Compact: snapshot after "before", then purge the log it covers.
    write(&leader, "before").await;
    let snapshot_index = leader.metrics().borrow().last_applied.unwrap().index;
    leader.raft().trigger().snapshot().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        metrics.wait_for(|m| m.snapshot.is_some_and(|s| s.index >= snapshot_index)),
    )
    .await
    .unwrap()
    .unwrap();
    leader
        .raft()
        .trigger()
        .purge_log(snapshot_index)
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        metrics.wait_for(|m| m.purged.is_some_and(|p| p.index >= snapshot_index)),
    )
    .await
    .unwrap()
    .unwrap();

    // The leader applies past its snapshot, as any busy council does.
    for key in ["after-1", "after-2", "after-3"] {
        write(&leader, key).await;
    }

    // A new member joins: its only way in is the stored snapshot.
    let learner = start(2, &router).await;
    tokio::time::timeout(Duration::from_secs(10), leader.add_learner(2, info(2)))
        .await
        .expect("adding the learner timed out")
        .unwrap();
    let leader_applied = leader.metrics().borrow().last_applied;
    let mut learner_metrics = learner.metrics();
    tokio::time::timeout(
        Duration::from_secs(10),
        learner_metrics.wait_for(|m| m.last_applied >= leader_applied),
    )
    .await
    .expect("the learner never caught up")
    .unwrap();

    let state = learner.desired_state().await;
    for key in ["before", "after-1", "after-2", "after-3"] {
        assert!(
            state.config.contains_key(key),
            "the learner is missing {key:?}: the snapshot it installed claimed entries it \
             didn't hold, so they were never replicated"
        );
    }

    let _ = learner.shutdown().await;
    let _ = leader.shutdown().await;
}
