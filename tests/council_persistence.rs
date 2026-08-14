//! Consensus persistence safety (CP3, Phase 12b.1).
//!
//! Acceptance shape: compact → corrupt → restart returns an error instead of
//! an empty cluster state. These tests drive a real openraft node against the
//! durable stores (log compaction included), then restart through
//! `cluster::runtime::open_raft_storage` — the exact storage-open path
//! `cluster::runtime::start` uses — so a corrupt or missing snapshot refuses
//! startup rather than booting a convincing, empty impostor of the cluster.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use redb::{Database, ReadableTable, TableDefinition};
use reliaburger::cluster::runtime::open_raft_storage;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo, RaftRequest};

/// Same table and keys as `council::state_machine` uses on disk — redeclared
/// here so the test can tamper with the persisted snapshot the way bit-rot or
/// a torn write would.
const SNAPSHOT: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_snapshot");
const SNAP_DATA_KEY: &str = "data";
const SNAP_VERSION_KEY: &str = "version";
const SNAP_CHECKSUM_KEY: &str = "checksum";

fn node_info() -> CouncilNodeInfo {
    CouncilNodeInfo {
        addr: "127.0.0.1:9444".parse().unwrap(),
        name: "n1".to_string(),
    }
}

/// First "run": bootstrap a single-node council on durable storage, write
/// some config, snapshot, and purge the compacted log prefix. Everything is
/// dropped before returning so the redb files can be reopened.
async fn run_and_compact(raft_dir: &Path) {
    let (log_store, store_fresh, state_machine) = open_raft_storage(raft_dir, None).await.unwrap();
    assert!(store_fresh, "a brand new store must read as fresh");

    let router = InMemoryRaftRouter::new();
    let network = InMemoryRaftNetworkFactory::new(1, router.clone());
    let config = CouncilConfig {
        // Purge everything the snapshot covers, immediately.
        max_in_snapshot_log_to_keep: 0,
        ..CouncilConfig::default()
    };
    let node = CouncilNode::new(1, config, network, log_store, state_machine, None)
        .await
        .unwrap();
    router.register(1, node.raft().clone()).await;
    node.initialize(BTreeMap::from([(1, node_info())]))
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !node.is_leader().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "single node never became leader"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    for i in 0..5 {
        node.write(RaftRequest::ConfigSet {
            key: format!("key-{i}"),
            value: format!("value-{i}"),
        })
        .await
        .unwrap();
    }

    // Compact: snapshot the applied state, then purge the covered log.
    node.raft().trigger().snapshot().await.unwrap();
    let metrics = node.raft().metrics();
    loop {
        let (snapshot, purged) = {
            let m = metrics.borrow();
            (m.snapshot, m.purged)
        };
        if let Some(snapshot) = snapshot {
            // Belt and braces: ask for the purge explicitly as well.
            node.raft().trigger().purge_log(snapshot.index).await.ok();
            if purged.map(|p| p.index) >= Some(snapshot.index) {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "snapshot/purge never completed: {:?}",
            *metrics.borrow()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    node.raft().shutdown().await.unwrap();
}

/// Flip one byte in the middle of a stored snapshot value.
fn flip_stored_byte(snapshot_path: &Path, key: &str) {
    let db = Database::create(snapshot_path).unwrap();
    let wtx = db.begin_write().unwrap();
    {
        let mut t = wtx.open_table(SNAPSHOT).unwrap();
        let mut bytes = {
            let guard = t.get(key).unwrap().unwrap();
            guard.value().to_vec()
        };
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x01;
        t.insert(key, bytes.as_slice()).unwrap();
    }
    wtx.commit().unwrap();
}

/// Remove the snapshot entirely (payload and envelope), as if it were lost.
fn delete_stored_snapshot(snapshot_path: &Path) {
    let db = Database::create(snapshot_path).unwrap();
    let wtx = db.begin_write().unwrap();
    {
        let mut t = wtx.open_table(SNAPSHOT).unwrap();
        t.remove(SNAP_DATA_KEY).unwrap();
        t.remove(SNAP_VERSION_KEY).unwrap();
        t.remove(SNAP_CHECKSUM_KEY).unwrap();
    }
    wtx.commit().unwrap();
}

#[tokio::test]
async fn compacted_snapshot_restores_state_across_restart() {
    // Control case: compaction plus a healthy snapshot restarts cleanly and
    // the applied state is all there.
    let dir = tempfile::tempdir().unwrap();
    let raft_dir = dir.path().join("raft");
    run_and_compact(&raft_dir).await;

    let (_log_store, store_fresh, state_machine) =
        open_raft_storage(&raft_dir, None).await.unwrap();
    assert!(!store_fresh, "a restarted store must not read as fresh");
    let state = state_machine.desired_state().await;
    for i in 0..5 {
        assert_eq!(
            state.config.get(&format!("key-{i}")),
            Some(&format!("value-{i}")),
            "compacted state must survive the restart"
        );
    }
}

#[tokio::test]
async fn compacted_then_corrupted_snapshot_refuses_restart() {
    // The theme's acceptance test: compact → corrupt → restart returns an
    // error instead of an empty cluster state.
    let dir = tempfile::tempdir().unwrap();
    let raft_dir = dir.path().join("raft");
    run_and_compact(&raft_dir).await;
    flip_stored_byte(&raft_dir.join("snapshot.redb"), SNAP_DATA_KEY);

    let err = match open_raft_storage(&raft_dir, None).await {
        Ok(_) => panic!("a corrupt snapshot after compaction must refuse startup"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("checksum"),
        "the error should name the checksum failure, got: {err}"
    );
}

#[tokio::test]
async fn compacted_then_missing_snapshot_refuses_restart() {
    // The log purged its prefix but the snapshot that covered it is gone:
    // this must be a startup error, never a fresh bootstrap.
    let dir = tempfile::tempdir().unwrap();
    let raft_dir = dir.path().join("raft");
    run_and_compact(&raft_dir).await;
    delete_stored_snapshot(&raft_dir.join("snapshot.redb"));

    let err = match open_raft_storage(&raft_dir, None).await {
        Ok(_) => panic!("a missing snapshot after compaction must refuse startup"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("purged"),
        "the error should name the purge boundary, got: {err}"
    );
}
