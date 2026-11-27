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
    router.register(id, &node).await;
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

/// The leader's applied log id once its metrics have caught up with its
/// log. A write returns when the entry is applied, but the metrics watch is
/// published a moment later, so reading it straight after a write can still
/// see the previous position (or none at all).
async fn settled_applied(node: &CouncilNode) -> openraft::LogId<u64> {
    let mut metrics = node.metrics();
    let settled = tokio::time::timeout(
        Duration::from_secs(10),
        metrics.wait_for(|m| {
            m.last_log_index.is_some() && m.last_applied.map(|l| l.index) == m.last_log_index
        }),
    )
    .await
    .expect("the leader's metrics never caught up with its log")
    .unwrap();
    settled.last_applied.unwrap()
}

/// Start node 1 as a single-voter leader, write `before`, snapshot it and
/// purge the log the snapshot covers, so a member that joins later can only
/// get `before` from the snapshot.
async fn compacted_leader(router: &InMemoryRaftRouter) -> CouncilNode {
    let leader = start(1, router).await;
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

    write(&leader, "before").await;
    let snapshot_index = settled_applied(&leader).await.index;
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
    leader
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_learner_installing_a_snapshot_gets_every_entry_applied_after_it() {
    let router = InMemoryRaftRouter::new();
    let leader = compacted_leader(&router).await;

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
    let leader_applied = Some(settled_applied(&leader).await);
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

/// #2367: each snapshot chunk used to get openraft's default 200 ms, and a
/// chunk that missed it restarted the whole install from offset 0. On a link
/// where a chunk takes 400 ms the learner never caught up, so it was never
/// promoted and a council that lost a voter couldn't replace it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_learner_installs_a_snapshot_over_a_link_slower_than_a_heartbeat() {
    let router = InMemoryRaftRouter::new();
    let leader = compacted_leader(&router).await;
    router.delay_snapshots(Duration::from_millis(400)).await;

    let learner = start(2, &router).await;
    tokio::time::timeout(Duration::from_secs(10), leader.add_learner(2, info(2)))
        .await
        .expect("adding the learner timed out")
        .unwrap();
    let installed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if learner.desired_state().await.config.contains_key("before") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        installed.is_ok(),
        "the learner never installed the snapshot over a link where a chunk takes 400 ms"
    );

    let _ = learner.shutdown().await;
    let _ = leader.shutdown().await;
}

/// From #437 (#427): the stored snapshot keeps the membership as well as the
/// log position it captured, and a follower installing it can still replay
/// every entry after it.
#[tokio::test]
async fn a_current_snapshot_keeps_its_captured_log_position_and_membership() {
    use openraft::storage::RaftStateMachine;
    use openraft::{
        CommittedLeaderId, Entry, EntryPayload, LogId, Membership, RaftSnapshotBuilder,
    };
    use reliaburger::council::types::{DesiredState, TypeConfig};
    use std::collections::BTreeSet;

    let log = |index| LogId::new(CommittedLeaderId::new(1, 1), index);
    let mut leader = CouncilStateMachine::new();
    leader
        .apply([Entry::<TypeConfig> {
            log_id: log(1),
            payload: EntryPayload::Normal(RaftRequest::ConfigSet {
                key: "before".into(),
                value: "snapshot".into(),
            }),
        }])
        .await
        .unwrap();
    let captured = leader
        .get_snapshot_builder()
        .await
        .build_snapshot()
        .await
        .unwrap();
    let members = BTreeMap::from([(1, info(1))]);
    leader
        .apply([
            Entry {
                log_id: log(2),
                payload: EntryPayload::Normal(RaftRequest::ConfigSet {
                    key: "after".into(),
                    value: "snapshot".into(),
                }),
            },
            Entry {
                log_id: log(3),
                payload: EntryPayload::Membership(Membership::new(
                    vec![BTreeSet::from([1])],
                    members,
                )),
            },
        ])
        .await
        .unwrap();
    let snapshot = leader.get_current_snapshot().await.unwrap().unwrap();
    let payload: DesiredState = serde_json::from_slice(snapshot.snapshot.get_ref()).unwrap();
    assert_eq!(snapshot.meta, captured.meta);
    assert_eq!(snapshot.meta.last_log_id, payload.last_applied_log);
    assert_eq!(snapshot.meta.last_membership, payload.last_membership);
    let mut follower = CouncilStateMachine::new();
    follower
        .install_snapshot(&snapshot.meta, snapshot.snapshot)
        .await
        .unwrap();
    // Entries after the snapshot must still be eligible for replay.
    assert_eq!(follower.applied_state().await.unwrap().0, Some(log(1)));
    assert!(!follower.desired_state().await.config.contains_key("after"));
}
