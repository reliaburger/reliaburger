//! Full-council-loss disaster recovery (Phase 12b.2, findings D21/CP12).
//!
//! Self-healing keeps the council alive while a majority survives. This module
//! handles the case self-healing can't: every voter dies at once. The workers
//! keep running their workloads, but nothing can elect, schedule, or serve
//! cluster state. There is no quorum to heal from.
//!
//! Recovery is operator-triggered, never automatic, because it deliberately
//! discards the dead cluster's Raft history. A survivor restores the desired
//! state from the last sealed backup (or, if it was a voter, its own snapshot and
//! committed log), re-bootstraps a fresh single-voter Raft with a bumped recovery
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
        "{path} holds no committed council state, neither a snapshot nor a committed log entry: \
         this node was never a voter, or its stores were lost; restore from a sealed backup with \
         `--from`"
    )]
    NoDurableState { path: String },
    #[error("re-bootstrap: {0}")]
    Bootstrap(String),
    #[error("persist recovered snapshot: {0}")]
    Persist(String),
    #[error(
        "{path} holds a council this node still serves, not a fenced one; re-enrolling would \
         throw away a live voter's log (pass --force only if this node is on a stale epoch that \
         wasn't fenced)"
    )]
    NotFenced { path: String },
}

/// Offline re-enrolment of a voter fenced out by a recovery (#424): drop the
/// replaced council's Raft state (log, snapshot and fence record) so the next
/// start joins the current council as a fresh member and adopts its epoch.
///
/// Refuses unless the persisted fence record says the node is fenced, or
/// `force` is set. Returns the epoch that fenced it, when one was recorded.
/// Refuses while a running node holds the stores open.
pub fn reenrol_data_dir(data_dir: &Path, force: bool) -> Result<Option<u64>, RecoveryError> {
    let raft_dir = data_dir.join("raft");
    let fenced_by = crate::council::fence::RecoveryFence::read_fenced_by(&raft_dir)
        .map_err(|e| RecoveryError::OpenSource(e.to_string()))?;
    if fenced_by.is_none() && !force {
        return Err(RecoveryError::NotFenced {
            path: raft_dir.display().to_string(),
        });
    }
    super::recovery_storage::remove(&raft_dir)
        .map_err(|e| RecoveryError::Persist(format!("remove {}: {e}", raft_dir.display())))?;
    Ok(fenced_by)
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
/// `master_key` is required for `BackupUrl` (to unseal). `NodeDataDir`
/// needs it only when the cluster encrypts its Raft log at rest.
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
        RecoverySource::NodeDataDir(dir) => load_state_from_data_dir(dir, master_key).await,
    }
}

/// Rebuild the state a voter had committed from its Raft directory
/// (`{data_dir}/raft`): the durable snapshot, then every committed log entry
/// after it (#479).
///
/// A cluster takes its first snapshot only after `snapshot_threshold`
/// (10,000) entries, so a young one holds its whole state in the log. The
/// stores open the way a node start opens them (finishing an interrupted
/// recovery, checking the purge boundary), and the log needs `master_key`
/// when the cluster encrypts it. Entries past the node's commit point are
/// left out: the dead council never agreed them.
async fn load_state_from_data_dir(
    data_dir: &Path,
    master_key: Option<&[u8; 32]>,
) -> Result<DesiredState, RecoveryError> {
    use openraft::RaftLogReader;
    use openraft::storage::{RaftLogStorage, RaftStateMachine};

    let raft_dir = data_dir.join("raft");
    let no_state = || RecoveryError::NoDurableState {
        path: raft_dir.display().to_string(),
    };
    // Opening the stores would create empty ones, and an empty state machine
    // would "recover" into zero apps, no CAs and no tokens, reported as
    // success. Refuse before anything is created.
    if !raft_dir.join("log.redb").exists() && !raft_dir.join("snapshot.redb").exists() {
        return Err(no_state());
    }
    crate::compatibility::ensure_state_compatible(data_dir)
        .map_err(|e| RecoveryError::OpenSource(e.to_string()))?;
    let (mut log, _fresh, mut machine) =
        crate::cluster::runtime::open_raft_storage(&raft_dir, master_key.map(|k| k.to_vec()))
            .await
            .map_err(|e| RecoveryError::OpenSource(e.to_string()))?;
    let read_error = |e: openraft::StorageError<u64>| {
        let hint = if master_key.is_none() {
            " (a secured cluster encrypts its log: pass --master-key)"
        } else {
            ""
        };
        RecoveryError::OpenSource(format!("read the committed log: {e}{hint}"))
    };

    let holds_snapshot = machine.holds_snapshot().await;
    let applied = machine.read_desired(|state| state.last_applied_log).await;
    let first = applied.map_or(0, |log_id| log_id.index + 1);
    let committed = log.read_committed().await.map_err(read_error)?;
    let mut replayed = false;
    if let Some(committed) = committed.filter(|committed| committed.index >= first) {
        let entries = log
            .try_get_log_entries(first..=committed.index)
            .await
            .map_err(read_error)?;
        let complete = entries.len() as u64 == committed.index - first + 1
            && entries.last().map(|entry| entry.log_id) == Some(committed);
        if !complete {
            return Err(RecoveryError::OpenSource(format!(
                "the committed log from index {first} to {} is incomplete",
                committed.index
            )));
        }
        machine
            .apply(entries)
            .await
            .map_err(|e| RecoveryError::OpenSource(format!("replay the committed log: {e}")))?;
        replayed = true;
    }
    if !holds_snapshot && !replayed {
        return Err(no_state());
    }
    Ok(machine.desired_state().await)
}

/// Offline recovery: replace the node's Raft directory with one holding only
/// `state` as its snapshot, so the next normal start re-bootstraps a fresh
/// single-voter Raft from the restored state.
///
/// This is what `relish council recover` runs against a *stopped* survivor.
/// Refuses live stores. Installation is journalled and resumed at startup;
/// the previous directory remains beside the replacement for inspection.
pub fn recover_data_dir(data_dir: &Path, state: DesiredState) -> Result<(), RecoveryError> {
    crate::compatibility::ensure_state_compatible(data_dir)
        .map_err(|e| RecoveryError::Persist(e.to_string()))?;
    // The whole old directory moves aside, the fence record included: the
    // recovered snapshot is then the only source of this node's epoch.
    super::recovery_storage::replace(&data_dir.join("raft"), state)
        .map_err(|e| RecoveryError::Persist(e.to_string()))
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

    async fn write_fence(raft_dir: &Path, fenced: bool) {
        use crate::council::fence::{FenceSnapshot, FenceState, RecoveryFence};
        std::fs::create_dir_all(raft_dir).unwrap();
        std::fs::write(raft_dir.join("log.redb"), b"old council").unwrap();
        let state = if fenced {
            FenceState::Fenced { newer_epoch: 2 }
        } else {
            FenceState::Serving
        };
        RecoveryFence::new(
            FenceSnapshot { epoch: 1, state },
            Some(raft_dir.join(crate::council::fence::FENCE_FILE)),
        )
        .persist()
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn reenrol_drops_a_fenced_nodes_raft_state() {
        let dir = tempfile::tempdir().unwrap();
        write_fence(&dir.path().join("raft"), true).await;
        assert_eq!(reenrol_data_dir(dir.path(), false).unwrap(), Some(2));
        assert!(!dir.path().join("raft").exists());
    }

    #[tokio::test]
    async fn reenrol_refuses_a_serving_voter_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        write_fence(&dir.path().join("raft"), false).await;
        assert!(matches!(
            reenrol_data_dir(dir.path(), false),
            Err(RecoveryError::NotFenced { .. })
        ));
        assert!(dir.path().join("raft").join("log.redb").exists());
        assert_eq!(reenrol_data_dir(dir.path(), true).unwrap(), None);
        assert!(!dir.path().join("raft").exists());
    }

    #[tokio::test]
    async fn reenrol_refuses_a_store_a_running_node_holds_open() {
        let dir = tempfile::tempdir().unwrap();
        let raft = dir.path().join("raft");
        write_fence(&raft, true).await;
        std::fs::remove_file(raft.join("log.redb")).unwrap();
        let _live = redb::Database::create(raft.join("log.redb")).unwrap();
        assert!(matches!(
            reenrol_data_dir(dir.path(), false),
            Err(RecoveryError::Persist(_))
        ));
        assert!(raft.join("log.redb").exists());
        assert!(raft.join(crate::council::fence::FENCE_FILE).exists());
    }

    #[test]
    fn recover_data_dir_drops_the_old_fence_record() {
        let dir = tempfile::tempdir().unwrap();
        crate::compatibility::ensure_state_compatible(dir.path()).unwrap();
        let raft = dir.path().join("raft");
        std::fs::create_dir_all(&raft).unwrap();
        std::fs::write(
            raft.join(crate::council::fence::FENCE_FILE),
            br#"{"epoch":0,"fenced_by":null}"#,
        )
        .unwrap();
        recover_data_dir(dir.path(), DesiredState::default()).unwrap();
        assert!(!raft.join(crate::council::fence::FENCE_FILE).exists());
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
        crate::compatibility::ensure_state_compatible(dir.path()).unwrap();
        let snapshot_path = dir.path().join("raft").join("snapshot.redb");
        let db = std::sync::Arc::new(redb::Database::create(&snapshot_path).unwrap());
        let sm = CouncilStateMachine::with_store(db).unwrap();
        let loaded = sm.desired_state().await;
        assert_eq!(loaded.config.get("k").map(String::as_str), Some("v"));
        assert_eq!(loaded.recovery_epoch, 1);
        assert!(loaded.last_applied_log.is_none());
    }

    #[test]
    fn recovery_refuses_to_overwrite_unmarked_development_state() {
        let dir = tempfile::tempdir().unwrap();
        let raft = dir.path().join("raft");
        std::fs::create_dir(&raft).unwrap();
        let log = raft.join("log.redb");
        std::fs::write(&log, b"preserve old log").unwrap();
        assert!(recover_data_dir(dir.path(), DesiredState::default()).is_err());
        assert_eq!(std::fs::read(log).unwrap(), b"preserve old log");
        assert!(!dir.path().join(crate::compatibility::STATE_STAMP).exists());
    }

    fn config_entry(term: u64, index: u64, key: &str) -> openraft::Entry<TypeConfig> {
        openraft::Entry {
            log_id: openraft::LogId::new(openraft::CommittedLeaderId::new(term, 0), index),
            payload: openraft::EntryPayload::Normal(
                crate::council::types::RaftRequest::ConfigSet {
                    key: key.to_string(),
                    value: index.to_string(),
                },
            ),
        }
    }

    /// Write `entries` into `{data_dir}/raft/log.redb`, the way a voter's
    /// running council would, and record `committed` as its commit point.
    async fn write_voter_log(
        data_dir: &Path,
        key: Option<&[u8; 32]>,
        entries: Vec<openraft::Entry<TypeConfig>>,
        committed: Option<u64>,
    ) {
        use openraft::storage::RaftLogStorage;
        crate::compatibility::ensure_state_compatible(data_dir).unwrap();
        let raft = data_dir.join("raft");
        std::fs::create_dir_all(&raft).unwrap();
        let mut log = crate::council::durable_log::DurableLogStore::open_with_key(
            raft.join("log.redb"),
            key.map(|k| k.to_vec()),
        )
        .unwrap();
        let committed = committed.map(|index| {
            entries
                .iter()
                .find(|entry| entry.log_id.index == index)
                .unwrap()
                .log_id
        });
        log.write_entries(entries).unwrap();
        log.save_committed(committed).await.unwrap();
    }

    fn config_keys(state: &DesiredState) -> Vec<&str> {
        let mut keys: Vec<_> = state.config.keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    /// #479: a young cluster never crossed the 10,000-entry snapshot
    /// threshold, so its whole state is in the committed log. Recovery
    /// without `--from` replays it.
    #[tokio::test]
    async fn recovery_replays_a_committed_log_that_was_never_snapshotted() {
        let dir = tempfile::tempdir().unwrap();
        write_voter_log(
            dir.path(),
            None,
            vec![
                config_entry(1, 0, "a"),
                config_entry(1, 1, "b"),
                config_entry(2, 2, "c"),
            ],
            Some(2),
        )
        .await;
        let loaded = load_state_from_data_dir(dir.path(), None).await.unwrap();
        assert_eq!(config_keys(&loaded), ["a", "b", "c"]);
    }

    /// Entries past the node's commit point were never agreed by the dead
    /// council; replaying them could resurrect a write its client saw fail.
    #[tokio::test]
    async fn recovery_stops_at_the_commit_point() {
        let dir = tempfile::tempdir().unwrap();
        write_voter_log(
            dir.path(),
            None,
            vec![config_entry(1, 0, "a"), config_entry(1, 1, "uncommitted")],
            Some(0),
        )
        .await;
        let loaded = load_state_from_data_dir(dir.path(), None).await.unwrap();
        assert_eq!(config_keys(&loaded), ["a"]);
    }

    #[tokio::test]
    async fn recovery_refuses_a_log_with_nothing_committed() {
        let dir = tempfile::tempdir().unwrap();
        write_voter_log(dir.path(), None, vec![config_entry(1, 0, "a")], None).await;
        let err = load_state_from_data_dir(dir.path(), None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, RecoveryError::NoDurableState { .. }),
            "expected NoDurableState, got {err:?}"
        );
    }

    /// A snapshot covers only a prefix: the committed tail after it is
    /// replayed too, or recovery would silently drop up to 10,000 writes.
    #[tokio::test]
    async fn recovery_replays_the_committed_tail_after_the_snapshot() {
        use openraft::storage::{RaftSnapshotBuilder, RaftStateMachine};
        let dir = tempfile::tempdir().unwrap();
        crate::compatibility::ensure_state_compatible(dir.path()).unwrap();
        let raft = dir.path().join("raft");
        std::fs::create_dir_all(&raft).unwrap();
        {
            let db =
                std::sync::Arc::new(redb::Database::create(raft.join("snapshot.redb")).unwrap());
            let mut machine = CouncilStateMachine::with_store(db).unwrap();
            machine
                .apply(vec![config_entry(1, 0, "a"), config_entry(1, 1, "b")])
                .await
                .unwrap();
            machine
                .get_snapshot_builder()
                .await
                .build_snapshot()
                .await
                .unwrap();
        }
        write_voter_log(
            dir.path(),
            None,
            vec![config_entry(1, 2, "c"), config_entry(1, 3, "d")],
            Some(3),
        )
        .await;
        let loaded = load_state_from_data_dir(dir.path(), None).await.unwrap();
        assert_eq!(config_keys(&loaded), ["a", "b", "c", "d"]);
    }

    /// A secured cluster encrypts its log with the master key, so replay
    /// needs it; without it recovery refuses instead of losing entries.
    #[tokio::test]
    async fn recovery_replays_an_encrypted_log_only_with_the_master_key() {
        let key = [7u8; 32];
        let dir = tempfile::tempdir().unwrap();
        write_voter_log(
            dir.path(),
            Some(&key),
            vec![config_entry(1, 0, "a")],
            Some(0),
        )
        .await;
        let err = load_state_from_data_dir(dir.path(), None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("--master-key"),
            "the refusal should name --master-key: {err}"
        );
        let loaded = load_state_from_data_dir(dir.path(), Some(&key))
            .await
            .unwrap();
        assert_eq!(config_keys(&loaded), ["a"]);
    }

    #[tokio::test]
    async fn recovery_refuses_a_data_dir_with_no_snapshot_file() {
        // A young cluster that never snapshotted: no snapshot.redb at all.
        let dir = tempfile::tempdir().unwrap();
        let err = load_state_from_data_dir(dir.path(), None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, RecoveryError::NoDurableState { .. }),
            "expected NoDurableState, got {err:?}"
        );
    }

    #[tokio::test]
    async fn recovery_refuses_a_snapshot_store_with_no_committed_blob() {
        // snapshot.redb exists but holds no snapshot blob (the normal state
        // before the first snapshot is written). Recovery must refuse rather
        // than load an empty DesiredState.
        let dir = tempfile::tempdir().unwrap();
        crate::compatibility::ensure_state_compatible(dir.path()).unwrap();
        let snapshot_path = dir.path().join("raft").join("snapshot.redb");
        std::fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();
        {
            let db = std::sync::Arc::new(redb::Database::create(&snapshot_path).unwrap());
            // Materialise the (empty) snapshot table, then drop the handle.
            let _ = CouncilStateMachine::with_store(db).unwrap();
        }

        let err = load_state_from_data_dir(dir.path(), None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, RecoveryError::NoDurableState { .. }),
            "expected NoDurableState, got {err:?}"
        );
    }

    #[tokio::test]
    async fn recovery_loads_a_genuine_snapshot() {
        // A node that WAS a voter and holds a real snapshot recovers its state.
        let dir = tempfile::tempdir().unwrap();
        let mut state = DesiredState::default();
        state.config.insert("k".to_string(), "v".to_string());
        recover_data_dir(dir.path(), state).unwrap();

        let loaded = load_state_from_data_dir(dir.path(), None).await.unwrap();
        assert_eq!(loaded.config.get("k").map(String::as_str), Some("v"));
    }

    /// #478: nodes refuse a catalogue generation below the newest they saw.
    /// The backup predates whatever the dead council published after it, so
    /// the recovered council starts its epoch's generations above every
    /// generation the replaced epoch could have reached.
    #[tokio::test]
    async fn a_recovered_council_publishes_above_the_epoch_it_replaces() {
        use crate::onion::withdrawal::EndpointWithdrawals;
        let mut backup = DesiredState::default();
        backup.endpoint_withdrawals.generation = 5;

        let dir = tempfile::tempdir().unwrap();
        recover_data_dir(dir.path(), backup.clone()).unwrap();
        let offline = load_state_from_data_dir(dir.path(), None).await.unwrap();
        let in_process = CouncilStateMachine::from_recovered_state(backup)
            .desired_state()
            .await;

        for state in [offline, in_process] {
            assert_eq!(state.recovery_epoch, 1);
            let generation = state.endpoint_withdrawals.generation;
            assert_eq!(generation, EndpointWithdrawals::epoch_floor(1));
            // Every generation epoch 0 could have published is below it.
            assert!(EndpointWithdrawals::epoch_floor(0) + u64::from(u32::MAX) < generation);
        }
    }

    /// A restored generation already past the new epoch's floor is never
    /// lowered: nodes may have seen it.
    #[test]
    fn entering_an_epoch_never_lowers_the_generation() {
        let mut withdrawals = crate::onion::withdrawal::EndpointWithdrawals {
            generation: u64::MAX - 1,
            ..Default::default()
        };
        withdrawals.enter_recovery_epoch(1);
        assert_eq!(withdrawals.generation, u64::MAX - 1);
    }

    #[tokio::test]
    async fn recover_data_dir_bumps_epoch_across_repeated_recoveries() {
        let dir = tempfile::tempdir().unwrap();
        recover_data_dir(dir.path(), DesiredState::default()).unwrap();

        // Load, then recover again from the loaded state.
        crate::compatibility::ensure_state_compatible(dir.path()).unwrap();
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
