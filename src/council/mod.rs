/// Raft consensus for the council (3–7 nodes).
///
/// The council is a small group of nodes that replicate desired state
/// (app specs, scheduling decisions, configuration) using the Raft
/// protocol. While Mustard gossip runs on all nodes for membership
/// and failure detection, Raft runs only on council members for
/// strong consistency.
///
/// We wrap the `openraft` crate with three adapter implementations:
/// - `MemLogStore` — in-memory Raft log storage
/// - `CouncilStateMachine` — applies entries to desired cluster state
/// - `InMemoryRaftRouter` — in-memory network for testing
pub mod apply;
pub mod backup;
pub mod durable_log;
pub mod log_store;
pub mod network;
pub mod node;
pub mod recovery;
pub mod selection;
pub mod state_machine;
pub mod types;

pub use apply::{config_to_desired_writes, config_to_leased_writes};
pub use node::CouncilNode;
pub use selection::{CouncilSelectionConfig, select_council_candidates};
pub use state_machine::SnapshotStoreError;
pub use types::{
    CouncilConfig, CouncilNodeInfo, CouncilResponse, DesiredState, RaftRequest, TypeConfig,
};

/// Validate the snapshot/log purge boundary before Raft starts.
///
/// Once the durable log purges its compacted prefix, the persisted snapshot
/// is the only copy of that state. A log that records a purge past what the
/// loaded snapshot covers (or with no snapshot at all) cannot be
/// reconstructed, so startup must refuse rather than silently boot with a
/// gap in the applied state (CP3).
// openraft's `StorageError` inside `SnapshotStoreError` is large but dictated
// by the crate; boxing it here buys nothing.
#[allow(clippy::result_large_err)]
pub async fn validate_purge_boundary(
    log_store: &durable_log::DurableLogStore,
    state_machine: &state_machine::CouncilStateMachine,
) -> Result<(), SnapshotStoreError> {
    let Some(purged) = log_store.last_purged_log_id()? else {
        return Ok(());
    };
    match state_machine.snapshot_last_applied().await {
        Some(applied) if applied.index >= purged.index => Ok(()),
        Some(applied) => Err(SnapshotStoreError::PurgedBeyondSnapshot {
            purged_index: purged.index,
            snapshot_index: applied.index,
        }),
        None => Err(SnapshotStoreError::PurgedWithoutSnapshot {
            purged_index: purged.index,
        }),
    }
}

/// Errors from the council subsystem.
#[derive(Debug, thiserror::Error)]
pub enum CouncilError {
    #[error("raft fatal: {0}")]
    Fatal(String),
    #[error("not leader, leader is node {leader:?}")]
    ForwardToLeader { leader: Option<u64> },
    #[error("raft initialisation failed: {0}")]
    InitError(String),
    #[error("raft write failed: {0}")]
    WriteFailed(String),
    #[error("linearizable read failed: {0}")]
    ReadFailed(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("security operation failed: {0}")]
    SecurityError(String),
}

#[cfg(test)]
mod tests {
    use openraft::storage::{RaftLogStorage, RaftStateMachine};
    use openraft::{CommittedLeaderId, EntryPayload, LogId, RaftSnapshotBuilder};

    use super::durable_log::DurableLogStore;
    use super::state_machine::CouncilStateMachine;
    use super::*;

    fn log_id(term: u64, index: u64) -> LogId<u64> {
        LogId::new(CommittedLeaderId::new(term, 0), index)
    }

    fn blank_entry(term: u64, index: u64) -> openraft::Entry<TypeConfig> {
        openraft::Entry {
            log_id: log_id(term, index),
            payload: EntryPayload::Blank,
        }
    }

    /// A durable log store purged up to `index`.
    async fn purged_log_store(dir: &std::path::Path, index: u64) -> DurableLogStore {
        let mut store = DurableLogStore::open(dir.join("log.redb")).unwrap();
        store.purge(log_id(1, index)).await.unwrap();
        store
    }

    /// A state machine whose snapshot covers up to `index` (None = no snapshot).
    async fn snapshotted_state_machine(index: Option<u64>) -> CouncilStateMachine {
        let mut sm = CouncilStateMachine::new();
        if let Some(index) = index {
            sm.apply(vec![blank_entry(1, index)]).await.unwrap();
            let mut builder = sm.get_snapshot_builder().await;
            builder.build_snapshot().await.unwrap();
        }
        sm
    }

    #[tokio::test]
    async fn purge_boundary_accepts_fresh_stores() {
        let dir = tempfile::tempdir().unwrap();
        let log = DurableLogStore::open(dir.path().join("log.redb")).unwrap();
        let sm = snapshotted_state_machine(None).await;
        assert!(validate_purge_boundary(&log, &sm).await.is_ok());
    }

    #[tokio::test]
    async fn purge_boundary_accepts_a_snapshot_covering_the_purged_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let log = purged_log_store(dir.path(), 3).await;
        let sm = snapshotted_state_machine(Some(5)).await;
        assert!(validate_purge_boundary(&log, &sm).await.is_ok());
    }

    #[tokio::test]
    async fn purge_boundary_rejects_a_missing_snapshot_after_compaction() {
        // The log says entries up to 3 were compacted away, but there is no
        // snapshot holding that state: nothing can reconstruct it, so
        // startup must refuse instead of booting empty.
        let dir = tempfile::tempdir().unwrap();
        let log = purged_log_store(dir.path(), 3).await;
        let sm = snapshotted_state_machine(None).await;
        let err = validate_purge_boundary(&log, &sm).await.unwrap_err();
        assert!(
            matches!(
                err,
                SnapshotStoreError::PurgedWithoutSnapshot { purged_index: 3 }
            ),
            "expected purged-without-snapshot, got: {err}"
        );
    }

    #[tokio::test]
    async fn purge_boundary_rejects_a_stale_snapshot_after_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let log = purged_log_store(dir.path(), 5).await;
        let sm = snapshotted_state_machine(Some(3)).await;
        let err = validate_purge_boundary(&log, &sm).await.unwrap_err();
        assert!(
            matches!(
                err,
                SnapshotStoreError::PurgedBeyondSnapshot {
                    purged_index: 5,
                    snapshot_index: 3
                }
            ),
            "expected purged-beyond-snapshot, got: {err}"
        );
    }
}
