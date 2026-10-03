//! A voter of a young cluster, which never reached the 10,000-entry snapshot
//! threshold, recovers its council from its own committed log (#479).
use std::collections::BTreeMap;
use std::time::Duration;

use reliaburger::cluster::runtime::open_raft_storage;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::recovery::{RecoverySource, load_recovery_state, recover_data_dir};
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo, RaftRequest};

const WRITES: usize = 50;

/// Run a one-voter council on `data_dir`'s durable stores, with its log
/// encrypted under `key`, commit `WRITES` config entries, and stop it.
async fn run_a_young_voter(data_dir: &std::path::Path, key: [u8; 32]) {
    let (log, fresh, machine) = open_raft_storage(&data_dir.join("raft"), Some(key.to_vec()))
        .await
        .unwrap();
    assert!(fresh);
    let router = InMemoryRaftRouter::new();
    let node = CouncilNode::new(
        1,
        CouncilConfig::default(),
        InMemoryRaftNetworkFactory::new(1, router.clone()),
        log,
        machine,
        Some(key),
    )
    .await
    .unwrap();
    router.register(1, node.raft().clone()).await;
    let info = CouncilNodeInfo::new("127.0.0.1:9444".parse().unwrap(), "voter");
    node.initialize(BTreeMap::from([(1, info)])).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !node.is_leader().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the voter never led");
    for index in 0..WRITES {
        node.write(RaftRequest::ConfigSet {
            key: format!("key-{index}"),
            value: index.to_string(),
        })
        .await
        .unwrap();
    }
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_voter_that_never_snapshotted_recovers_from_its_committed_log() {
    let root = tempfile::tempdir().unwrap();
    reliaburger::compatibility::ensure_state_compatible(root.path()).unwrap();
    let key = [9u8; 32];
    run_a_young_voter(root.path(), key).await;

    let source = RecoverySource::NodeDataDir(root.path().to_path_buf());
    let state = load_recovery_state(&source, Some(&key))
        .await
        .expect("recovery refused a voter whose state is all in its log");
    assert_eq!(state.config.len(), WRITES);
    assert_eq!(state.config.get("key-49").map(String::as_str), Some("49"));

    // The recovered directory now holds that state as its snapshot.
    recover_data_dir(root.path(), state).unwrap();
    let (_log, fresh, machine) = open_raft_storage(&root.path().join("raft"), Some(key.to_vec()))
        .await
        .unwrap();
    assert!(fresh);
    let restored = machine.desired_state().await;
    assert_eq!(restored.config.len(), WRITES);
    assert_eq!(restored.recovery_epoch, 1);
}
