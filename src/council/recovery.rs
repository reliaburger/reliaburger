//! Full-council-loss disaster recovery (Phase 12b.2, findings D21/CP12).
//!
//! Self-healing keeps the council alive while a majority survives. This module
//! handles the case self-healing can't: every voter dies at once. The workers
//! keep running their workloads, but nothing can elect, schedule, or serve
//! cluster state. There is no quorum to heal from.
//!
//! Recovery is operator-triggered, never automatic, because it deliberately
//! discards the dead cluster's Raft history. A survivor restores the desired
//! state from the last sealed backup (or its own durable snapshot if it was a
//! voter), re-bootstraps a fresh single-voter Raft with a bumped recovery
//! epoch, and the #88 reconciler regrows the council from healthy members. The
//! cost is honest: anything written after the last backup is lost.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use openraft::RaftNetworkFactory;

use crate::council::node::CouncilNode;
use crate::council::state_machine::CouncilStateMachine;
use crate::council::types::{CouncilConfig, CouncilNodeInfo, DesiredState, TypeConfig};
use crate::mustard::membership::MembershipSnapshot;

/// Where the restored desired state comes from.
#[derive(Debug, Clone)]
pub enum RecoverySource {
    /// A sealed backup at an object-store URL (`file://`, `s3://`, `gs://`).
    BackupUrl(String),
    /// This node's own durable Raft data directory (it was a voter).
    NodeDataDir(std::path::PathBuf),
}

/// Errors from the recovery flow.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("a live council still exists: {0}; refusing to recover without --force")]
    LiveCouncil(String),
    #[error("backup: {0}")]
    Backup(#[from] crate::council::backup::BackupError),
    #[error("no backup found at {url}")]
    NoBackup { url: String },
    #[error("open recovery source: {0}")]
    OpenSource(String),
    #[error(
        "no committed snapshot exists at {path}: this node holds no durable state to recover from \
         (a young or low-churn cluster may never have snapshotted — restore from a sealed backup \
         with `--from` instead of the node's data directory)"
    )]
    EmptySnapshot { path: String },
    #[error("re-bootstrap: {0}")]
    Bootstrap(String),
    #[error("persist recovered snapshot: {0}")]
    Persist(String),
}

/// Whether the gossip view shows any council voter still alive.
///
/// This is the pre-flight guard for `relish council recover`: recovery
/// destroys the old term line, so running it against a cluster that still has
/// a quorum would split-brain. `--force` overrides this, but only loudly.
///
/// Returns the name of a live voter if one is found, so the refusal message
/// can point at it.
pub fn live_council_voter(members: &[MembershipSnapshot]) -> Option<String> {
    members
        .iter()
        .find(|m| m.is_council && !m.state.is_down())
        .map(|m| m.node_id.0.clone())
}

/// Load and unseal a `DesiredState` from a recovery source.
///
/// `master_key` is required for `BackupUrl` (to unseal); `NodeDataDir` reads
/// the plaintext durable snapshot the node already trusts, so it ignores the
/// key.
pub async fn load_recovery_state(
    source: &RecoverySource,
    master_key: Option<&[u8; 32]>,
) -> Result<DesiredState, RecoveryError> {
    match source {
        RecoverySource::BackupUrl(url) => {
            let store = crate::council::backup::BackupStore::from_url(url)?;
            let sealed = store
                .latest()
                .await?
                .ok_or_else(|| RecoveryError::NoBackup { url: url.clone() })?;
            let key = master_key.ok_or_else(|| {
                RecoveryError::OpenSource(
                    "a sealed backup needs the cluster master key".to_string(),
                )
            })?;
            let bytes = crate::council::backup::unseal_snapshot(key, &sealed)?;
            serde_json::from_slice(&bytes)
                .map_err(|e| RecoveryError::OpenSource(format!("decode backup payload: {e}")))
        }
        RecoverySource::NodeDataDir(dir) => load_state_from_data_dir(dir).await,
    }
}

/// Read the durable Raft snapshot under `{data_dir}/raft/snapshot.redb` into a
/// `DesiredState`. Used when the recovering node was itself a voter and still
/// holds the most recent committed state.
async fn load_state_from_data_dir(data_dir: &Path) -> Result<DesiredState, RecoveryError> {
    let snapshot_path = data_dir.join("raft").join("snapshot.redb");
    // `redb::Database::create` would fabricate an empty store for a missing
    // file, and `with_store` returns empty state when no snapshot blob has
    // been written yet — so a young cluster would "recover" into zero apps,
    // no CAs and no tokens, reported as success. Refuse both cases.
    if !snapshot_path.exists() {
        return Err(RecoveryError::EmptySnapshot {
            path: snapshot_path.display().to_string(),
        });
    }
    let db = std::sync::Arc::new(
        redb::Database::create(&snapshot_path)
            .map_err(|e| RecoveryError::OpenSource(format!("open snapshot store: {e}")))?,
    );
    if !CouncilStateMachine::snapshot_present(&db)
        .map_err(|e| RecoveryError::OpenSource(format!("inspect snapshot store: {e}")))?
    {
        return Err(RecoveryError::EmptySnapshot {
            path: snapshot_path.display().to_string(),
        });
    }
    let state_machine = CouncilStateMachine::with_store(db)
        .map_err(|e| RecoveryError::OpenSource(format!("load snapshot: {e}")))?;
    Ok(state_machine.desired_state().await)
}

/// Offline recovery: stamp `state` into the node's durable snapshot store and
/// wipe the dead cluster's log, so the next normal start re-bootstraps a fresh
/// single-voter Raft from the restored state.
///
/// This is what `relish council recover` runs against a *stopped* survivor.
/// The node must not be running (redb takes an exclusive lock), which is the
/// operator's responsibility.
pub fn recover_data_dir(data_dir: &Path, state: DesiredState) -> Result<(), RecoveryError> {
    let raft_dir = data_dir.join("raft");
    std::fs::create_dir_all(&raft_dir)
        .map_err(|e| RecoveryError::Persist(format!("create raft dir: {e}")))?;

    // Wipe the dead cluster's log first: its term line and membership belong to
    // the council that died. A fresh (absent) log makes the node bootstrap.
    let log_path = raft_dir.join("log.redb");
    if log_path.exists() {
        std::fs::remove_file(&log_path)
            .map_err(|e| RecoveryError::Persist(format!("remove stale log: {e}")))?;
    }

    let snapshot_path = raft_dir.join("snapshot.redb");
    // Replace any existing snapshot store so the recovered state is the only
    // state the node loads.
    if snapshot_path.exists() {
        std::fs::remove_file(&snapshot_path)
            .map_err(|e| RecoveryError::Persist(format!("remove stale snapshot: {e}")))?;
    }
    let db = redb::Database::create(&snapshot_path)
        .map_err(|e| RecoveryError::Persist(format!("create snapshot store: {e}")))?;
    CouncilStateMachine::persist_recovered_snapshot(&db, state)
        .map_err(|e| RecoveryError::Persist(format!("write recovered snapshot: {e}")))?;
    Ok(())
}

/// In-process recovery: build a `CouncilNode` seeded from a restored
/// `DesiredState` and initialise it as the sole voter.
///
/// The caller supplies the network factory and log store (a fresh one), so the
/// same helper serves both the production TCP transport and the in-memory test
/// router. The returned node bumps the recovery epoch and leads immediately
/// (quorum of one).
pub async fn recover_from_desired_state<N, LS>(
    raft_id: u64,
    info: CouncilNodeInfo,
    config: CouncilConfig,
    network: N,
    log_store: LS,
    state: DesiredState,
) -> Result<CouncilNode, RecoveryError>
where
    N: RaftNetworkFactory<TypeConfig>,
    LS: openraft::storage::RaftLogStorage<TypeConfig>,
{
    let state_machine = CouncilStateMachine::from_recovered_state(state);
    let node = CouncilNode::new(raft_id, config, network, log_store, state_machine, None)
        .await
        .map_err(|e| RecoveryError::Bootstrap(e.to_string()))?;

    let mut members = BTreeMap::new();
    members.insert(raft_id, info);
    node.initialize(members)
        .await
        .map_err(|e| RecoveryError::Bootstrap(e.to_string()))?;
    Ok(node)
}

/// The voter set a freshly recovered council starts with: just itself.
pub fn sole_voter_set(raft_id: u64) -> BTreeSet<u64> {
    BTreeSet::from([raft_id])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mustard::state::NodeState;
    use std::net::SocketAddr;

    fn snap(name: &str, is_council: bool, state: NodeState) -> MembershipSnapshot {
        MembershipSnapshot {
            node_id: crate::meat::NodeId::new(name),
            address: SocketAddr::from(([127, 0, 0, 1], 9443)),
            state,
            incarnation: 1,
            is_council,
            is_leader: false,
            labels: BTreeMap::new(),
            first_seen: std::time::Instant::now(),
            resources: None,
        }
    }

    #[test]
    fn live_council_voter_found_when_council_member_alive() {
        let members = vec![
            snap("worker", false, NodeState::Alive),
            snap("voter", true, NodeState::Alive),
        ];
        assert_eq!(live_council_voter(&members).as_deref(), Some("voter"));
    }

    #[test]
    fn live_council_voter_none_when_all_voters_down() {
        let members = vec![
            snap("worker", false, NodeState::Alive),
            snap("dead-voter", true, NodeState::Dead),
        ];
        assert!(live_council_voter(&members).is_none());
    }

    #[test]
    fn live_council_voter_ignores_alive_workers() {
        let members = vec![snap("worker", false, NodeState::Alive)];
        assert!(live_council_voter(&members).is_none());
    }

    #[test]
    fn sole_voter_set_is_just_self() {
        assert_eq!(sole_voter_set(42), BTreeSet::from([42]));
    }

    #[tokio::test]
    async fn recover_data_dir_writes_a_loadable_recovered_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = DesiredState::default();
        state.config.insert("k".to_string(), "v".to_string());

        recover_data_dir(dir.path(), state).unwrap();

        // The next start's loader sees the restored state with a bumped epoch
        // and no stale log id.
        let snapshot_path = dir.path().join("raft").join("snapshot.redb");
        let db = std::sync::Arc::new(redb::Database::create(&snapshot_path).unwrap());
        let sm = CouncilStateMachine::with_store(db).unwrap();
        let loaded = sm.desired_state().await;
        assert_eq!(loaded.config.get("k").map(String::as_str), Some("v"));
        assert_eq!(loaded.recovery_epoch, 1);
        assert!(loaded.last_applied_log.is_none());
    }

    #[tokio::test]
    async fn recovery_refuses_a_data_dir_with_no_snapshot_file() {
        // A young cluster that never snapshotted: no snapshot.redb at all.
        let dir = tempfile::tempdir().unwrap();
        let err = load_state_from_data_dir(dir.path()).await.unwrap_err();
        assert!(
            matches!(err, RecoveryError::EmptySnapshot { .. }),
            "expected EmptySnapshot, got {err:?}"
        );
    }

    #[tokio::test]
    async fn recovery_refuses_a_snapshot_store_with_no_committed_blob() {
        // snapshot.redb exists but holds no snapshot blob (the normal state
        // before the first snapshot is written). Recovery must refuse rather
        // than load an empty DesiredState.
        let dir = tempfile::tempdir().unwrap();
        let snapshot_path = dir.path().join("raft").join("snapshot.redb");
        std::fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();
        {
            let db = std::sync::Arc::new(redb::Database::create(&snapshot_path).unwrap());
            // Materialise the (empty) snapshot table, then drop the handle.
            let _ = CouncilStateMachine::with_store(db).unwrap();
        }

        let err = load_state_from_data_dir(dir.path()).await.unwrap_err();
        assert!(
            matches!(err, RecoveryError::EmptySnapshot { .. }),
            "expected EmptySnapshot, got {err:?}"
        );
    }

    #[tokio::test]
    async fn recovery_loads_a_genuine_snapshot() {
        // A node that WAS a voter and holds a real snapshot recovers its state.
        let dir = tempfile::tempdir().unwrap();
        let mut state = DesiredState::default();
        state.config.insert("k".to_string(), "v".to_string());
        recover_data_dir(dir.path(), state).unwrap();

        let loaded = load_state_from_data_dir(dir.path()).await.unwrap();
        assert_eq!(loaded.config.get("k").map(String::as_str), Some("v"));
    }

    #[tokio::test]
    async fn recover_data_dir_bumps_epoch_across_repeated_recoveries() {
        let dir = tempfile::tempdir().unwrap();
        recover_data_dir(dir.path(), DesiredState::default()).unwrap();

        // Load, then recover again from the loaded state.
        let snapshot_path = dir.path().join("raft").join("snapshot.redb");
        let first = {
            let db = std::sync::Arc::new(redb::Database::create(&snapshot_path).unwrap());
            let sm = CouncilStateMachine::with_store(db).unwrap();
            sm.desired_state().await
        };
        assert_eq!(first.recovery_epoch, 1);

        recover_data_dir(dir.path(), first).unwrap();
        let db = std::sync::Arc::new(redb::Database::create(&snapshot_path).unwrap());
        let sm = CouncilStateMachine::with_store(db).unwrap();
        let second = sm.desired_state().await;
        assert_eq!(second.recovery_epoch, 2);
    }
}
