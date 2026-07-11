//! Raft state machine that maintains desired cluster state.
//!
//! Applies `RaftRequest` entries to an in-memory `DesiredState` and
//! supports JSON-based snapshots for follower catch-up.

use std::io::Cursor;
use std::sync::Arc;

use openraft::storage::RaftStateMachine;
use openraft::{
    EntryPayload, LogId, RaftSnapshotBuilder, Snapshot, SnapshotMeta, StorageError, StorageIOError,
    StoredMembership,
};
use redb::{Database, TableDefinition};
use tokio::sync::RwLock;

use super::types::{CouncilNodeInfo, CouncilResponse, DesiredState, RaftRequest, TypeConfig};

/// Persisted snapshot: `data` = JSON of `DesiredState` (which itself carries
/// `last_applied_log` + `last_membership`), `index` = snapshot counter.
const SNAPSHOT: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_snapshot");
const SNAP_DATA_KEY: &str = "data";
const SNAP_INDEX_KEY: &str = "index";

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StateMachineInner {
    state: DesiredState,
    snapshot_index: u64,
    snapshot_data: Option<Vec<u8>>,
    /// When set, snapshots are persisted here so applied state survives a
    /// restart (the durable log replays only the post-snapshot tail).
    db: Option<Arc<Database>>,
}

// Manual Debug: `redb::Database` isn't `Debug`, and its contents aren't useful
// to print anyway.
impl std::fmt::Debug for StateMachineInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateMachineInner")
            .field("snapshot_index", &self.snapshot_index)
            .field("has_snapshot", &self.snapshot_data.is_some())
            .field("durable", &self.db.is_some())
            .finish()
    }
}

/// Write the latest snapshot (data + index) to redb, fsyncing on commit.
// `redb::Error` is large but dictated by the crate; boxing it here buys nothing.
#[allow(clippy::result_large_err)]
fn persist_snapshot(db: &Database, data: &[u8], index: u64) -> Result<(), redb::Error> {
    let wtx = db.begin_write()?;
    {
        let mut t = wtx.open_table(SNAPSHOT)?;
        t.insert(SNAP_DATA_KEY, data)?;
        t.insert(SNAP_INDEX_KEY, index.to_le_bytes().as_slice())?;
    }
    wtx.commit()?;
    Ok(())
}

impl StateMachineInner {
    /// Apply a request. Returns a request-specific response for entries
    /// that carry a verdict back to the proposer (`AllocateSerial` gets
    /// its serial, `GcReport` gets the approved deletions); `None` means
    /// the generic `Applied` response.
    fn apply_request(&mut self, request: &RaftRequest) -> Option<CouncilResponse> {
        match request {
            RaftRequest::AppSpec { app_id, spec } => {
                self.state.apps.insert(app_id.clone(), *spec.clone());
            }
            RaftRequest::AppDelete { app_id } => {
                self.state.apps.remove(app_id);
                self.state.scheduling.remove(app_id);
            }
            RaftRequest::SchedulingDecision(decision) => {
                self.state
                    .scheduling
                    .insert(decision.app_id.clone(), decision.placements.clone());
            }
            RaftRequest::ConfigSet { key, value } => {
                self.state.config.insert(key.clone(), value.clone());
            }
            RaftRequest::ManifestCommit(commit) => {
                self.state.manifest_catalog.apply_manifest_commit(commit);
            }
            RaftRequest::UpdateLayerLocations(update) => {
                self.state.manifest_catalog.apply_update_locations(update);
            }
            RaftRequest::GcReport(report) => {
                // The state machine is the deletion arbiter (M2): apply
                // runs serialised through the Raft log, so two nodes
                // racing to delete the last two copies of a layer get
                // their reports arbitrated in order — the second one is
                // refused the digest that would lose its final holder.
                let approved = self.state.manifest_catalog.apply_gc_report(report);
                return Some(CouncilResponse::GcApproved { approved });
            }
            RaftRequest::DeleteTag(delete) => {
                self.state.manifest_catalog.apply_delete_tag(delete);
            }
            RaftRequest::DeployUpdate { app_id, state } => {
                let key = app_id.to_string();
                if let Some((_, existing)) = self
                    .state
                    .active_deploys
                    .iter_mut()
                    .find(|(k, _)| k == &key)
                {
                    *existing = *state.clone();
                } else {
                    self.state.active_deploys.push((key, *state.clone()));
                }
            }
            RaftRequest::DeployComplete { app_id, entry } => {
                let key = app_id.to_string();
                // Remove from active
                self.state.active_deploys.retain(|(k, _)| k != &key);
                // Add to history (cap at 50 per app)
                if let Some((_, history)) = self
                    .state
                    .deploy_history
                    .iter_mut()
                    .find(|(k, _)| k == &key)
                {
                    history.push(entry.clone());
                    if history.len() > 50 {
                        history.remove(0);
                    }
                } else {
                    self.state.deploy_history.push((key, vec![entry.clone()]));
                }
            }
            RaftRequest::AutoscaleOverride {
                app_id,
                replicas,
                reason: _,
            } => {
                let key = app_id.to_string();
                if let Some((_, existing)) = self
                    .state
                    .autoscale_overrides
                    .iter_mut()
                    .find(|(k, _)| k == &key)
                {
                    *existing = *replicas;
                } else {
                    self.state.autoscale_overrides.push((key, *replicas));
                }
            }
            RaftRequest::GitOpsCoordinatorElection(election) => {
                self.state.gitops_coordinator = Some(election.clone());
            }
            RaftRequest::GitOpsSyncUpdate(sync_state) => {
                self.state.gitops_sync_state = Some(*sync_state.clone());
            }
            RaftRequest::AttachSignature(attach) => {
                self.state.manifest_catalog.apply_attach_signature(attach);
            }
            RaftRequest::SecurityStateInit(ss) => {
                self.state.security_state = *ss.clone();
            }
            RaftRequest::CreateJoinToken(jt) => {
                self.state.security_state.join_tokens.push(jt.clone());
            }
            RaftRequest::ConsumeJoinToken { token_hash } => {
                if let Some(jt) = self
                    .state
                    .security_state
                    .join_tokens
                    .iter_mut()
                    .find(|jt| jt.token_hash == *token_hash)
                {
                    jt.consumed = true;
                }
            }
            RaftRequest::CreateApiToken(token) => {
                self.state.security_state.api_tokens.push(token.clone());
            }
            RaftRequest::RevokeApiToken { name } => {
                self.state
                    .security_state
                    .api_tokens
                    .retain(|t| t.name != *name);
            }
            RaftRequest::AllocateSerial => {
                // Return the pre-increment value as this entry's serial.
                let serial = self.state.security_state.next_serial;
                self.state.security_state.next_serial += 1;
                return Some(CouncilResponse::SerialAllocated { serial });
            }
            RaftRequest::RotateSecretKey { scope, new_keypair } => {
                // Mark existing keypairs with the same scope as read-only
                for kp in &mut self.state.security_state.age_keypairs {
                    if kp.scope == *scope {
                        kp.read_only = true;
                    }
                }
                // Add the new keypair
                self.state
                    .security_state
                    .age_keypairs
                    .push(new_keypair.clone());
            }
            RaftRequest::FinalizeSecretRotation { scope } => {
                // Retire the old (read-only) keys for this scope, but only if an
                // active replacement exists — never leave a scope with no
                // usable key, which would make its secrets permanently
                // undecryptable (PKI8). Re-encrypting existing ciphertext with
                // the active key before finalising is the operator's two-step
                // flow (`relish secret rotate` → re-encrypt → `--finalize`),
                // which now works because encryption selects the active key.
                let has_active = self
                    .state
                    .security_state
                    .age_keypairs
                    .iter()
                    .any(|kp| kp.scope == *scope && !kp.read_only);
                if has_active {
                    self.state
                        .security_state
                        .age_keypairs
                        .retain(|kp| kp.scope != *scope || !kp.read_only);
                }
            }
            RaftRequest::RevokeCertificate(entry) => {
                self.state.security_state.crl.entries.push(entry.clone());
                self.state.security_state.crl.version += 1;
                self.state.security_state.crl.updated_at = std::time::SystemTime::now();
            }
            RaftRequest::Noop => {}
            RaftRequest::UpgradeUpdate { state } => {
                // Last-writer-wins: only the leader's orchestrator writes,
                // and it always writes the full state.
                self.state.active_upgrade = Some(*state.clone());
            }
            RaftRequest::UpgradeClear { upgrade_id } => {
                if let Some(active) = self.state.active_upgrade.take() {
                    if active.upgrade_id == *upgrade_id {
                        self.state.upgrade_history.push(active);
                        if self.state.upgrade_history.len() > 20 {
                            self.state.upgrade_history.remove(0);
                        }
                    } else {
                        // Clear for a different id: put it back untouched.
                        self.state.active_upgrade = Some(active);
                    }
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// CouncilStateMachine
// ---------------------------------------------------------------------------

/// Raft state machine that applies entries to `DesiredState`.
///
/// Shared via `Arc<RwLock<_>>` so the snapshot builder can take a
/// read lock while the Raft core continues applying.
#[derive(Debug, Clone, Default)]
pub struct CouncilStateMachine {
    inner: Arc<RwLock<StateMachineInner>>,
}

impl CouncilStateMachine {
    /// Create a new empty in-memory state machine (tests).
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a state machine backed by `db`, loading any persisted snapshot.
    ///
    /// On restart the loaded snapshot restores the applied state up to its
    /// boundary; openraft then replays the durable log's post-snapshot tail.
    #[allow(clippy::result_large_err)]
    pub fn with_store(db: Arc<Database>) -> Result<Self, redb::Error> {
        // Materialise the table so reads on a fresh store don't error.
        let wtx = db.begin_write()?;
        {
            wtx.open_table(SNAPSHOT)?;
        }
        wtx.commit()?;

        let mut inner = StateMachineInner::default();
        {
            let rtx = db.begin_read()?;
            let t = rtx.open_table(SNAPSHOT)?;
            if let Some(data) = t.get(SNAP_DATA_KEY)? {
                let bytes = data.value().to_vec();
                // A snapshot blob that EXISTS but won't decode is corruption,
                // not absence. Fail closed (CP3): after log compaction the
                // entries this snapshot covers are gone, so silently booting an
                // empty state here would destroy the cluster's desired and
                // security state (app specs, CA material, tokens). Refuse to
                // start so an operator can restore from backup instead.
                let state = serde_json::from_slice::<DesiredState>(&bytes).map_err(|e| {
                    redb::StorageError::Corrupted(format!(
                        "council snapshot present but failed to decode: {e}"
                    ))
                })?;
                inner.state = state;
                inner.snapshot_data = Some(bytes);
            }
            if let Some(idx) = t.get(SNAP_INDEX_KEY)? {
                let v = idx.value();
                if v.len() == 8 {
                    inner.snapshot_index = u64::from_le_bytes(v.try_into().unwrap());
                }
            }
        }
        inner.db = Some(db);
        Ok(Self {
            inner: Arc::new(RwLock::new(inner)),
        })
    }

    /// Read the current desired state.
    pub async fn desired_state(&self) -> DesiredState {
        self.inner.read().await.state.clone()
    }
}

// ---------------------------------------------------------------------------
// RaftStateMachine
// ---------------------------------------------------------------------------

impl RaftStateMachine<TypeConfig> for CouncilStateMachine {
    type SnapshotBuilder = MemSnapshotBuilder;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId<u64>>, StoredMembership<u64, CouncilNodeInfo>), StorageError<u64>>
    {
        let guard = self.inner.read().await;
        Ok((
            guard.state.last_applied_log,
            guard.state.last_membership.clone(),
        ))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<CouncilResponse>, StorageError<u64>>
    where
        I: IntoIterator<Item = openraft::Entry<TypeConfig>> + Send,
        I::IntoIter: Send,
    {
        let mut guard = self.inner.write().await;
        let mut responses = Vec::new();

        for entry in entries {
            let log_id = entry.log_id;
            guard.state.last_applied_log = Some(log_id);

            match entry.payload {
                EntryPayload::Blank => {
                    responses.push(CouncilResponse::Applied {
                        log_index: log_id.index,
                    });
                }
                EntryPayload::Normal(request) => {
                    let response = guard.apply_request(&request);
                    responses.push(response.unwrap_or(CouncilResponse::Applied {
                        log_index: log_id.index,
                    }));
                }
                EntryPayload::Membership(membership) => {
                    guard.state.last_membership = StoredMembership::new(Some(log_id), membership);
                    responses.push(CouncilResponse::Applied {
                        log_index: log_id.index,
                    });
                }
            }
        }
        Ok(responses)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        MemSnapshotBuilder {
            inner: Arc::clone(&self.inner),
        }
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<u64>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<u64, CouncilNodeInfo>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<u64>> {
        let data = snapshot.into_inner();
        let new_state: DesiredState = serde_json::from_slice(&data)
            .map_err(|e| StorageError::from(StorageIOError::read_state_machine(&e)))?;

        let mut guard = self.inner.write().await;
        guard.state = new_state;
        guard.state.last_applied_log = meta.last_log_id;
        guard.state.last_membership = meta.last_membership.clone();
        guard.snapshot_index += 1;
        guard.snapshot_data = Some(data.clone());

        // Persist the installed snapshot so it survives a restart too.
        if let Some(db) = &guard.db {
            persist_snapshot(db, &data, guard.snapshot_index)
                .map_err(|e| StorageError::from(StorageIOError::write_state_machine(&e)))?;
        }
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<u64>> {
        let guard = self.inner.read().await;
        match &guard.snapshot_data {
            Some(data) => {
                let meta = SnapshotMeta {
                    last_log_id: guard.state.last_applied_log,
                    last_membership: guard.state.last_membership.clone(),
                    snapshot_id: format!("mem-{}", guard.snapshot_index),
                };
                Ok(Some(Snapshot {
                    meta,
                    snapshot: Box::new(Cursor::new(data.clone())),
                }))
            }
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// MemSnapshotBuilder
// ---------------------------------------------------------------------------

/// Builds a snapshot from the current state machine state.
#[derive(Debug)]
pub struct MemSnapshotBuilder {
    inner: Arc<RwLock<StateMachineInner>>,
}

impl RaftSnapshotBuilder<TypeConfig> for MemSnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<u64>> {
        let mut guard = self.inner.write().await;

        let data = serde_json::to_vec(&guard.state)
            .map_err(|e| StorageError::from(StorageIOError::read_state_machine(&e)))?;

        guard.snapshot_index += 1;
        let snapshot_id = format!("mem-{}", guard.snapshot_index);
        guard.snapshot_data = Some(data.clone());

        // Persist so applied state survives a restart.
        if let Some(db) = &guard.db {
            persist_snapshot(db, &data, guard.snapshot_index)
                .map_err(|e| StorageError::from(StorageIOError::write_state_machine(&e)))?;
        }

        let meta = SnapshotMeta {
            last_log_id: guard.state.last_applied_log,
            last_membership: guard.state.last_membership.clone(),
            snapshot_id,
        };

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io::Read;

    use openraft::Membership;

    use crate::config::app::AppSpec;
    use crate::meat::types::{AppId, NodeId, Placement, Resources, SchedulingDecision};

    use super::*;

    fn default_spec() -> AppSpec {
        toml::from_str(r#"image = "test:v1""#).unwrap()
    }

    fn log_id(term: u64, index: u64) -> LogId<u64> {
        LogId::new(openraft::CommittedLeaderId::new(term, 0), index)
    }

    fn normal_entry(term: u64, index: u64, request: RaftRequest) -> openraft::Entry<TypeConfig> {
        openraft::Entry {
            log_id: log_id(term, index),
            payload: EntryPayload::Normal(request),
        }
    }

    #[tokio::test]
    async fn apply_app_spec_adds_to_state() {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "prod");
        let spec = AppSpec {
            image: Some("myapp:v2".to_string()),
            ..default_spec()
        };
        let entry = normal_entry(
            1,
            1,
            RaftRequest::AppSpec {
                app_id: app_id.clone(),
                spec: Box::new(spec.clone()),
            },
        );

        let responses = sm.apply(vec![entry]).await.unwrap();
        assert_eq!(responses.len(), 1);

        let state = sm.desired_state().await;
        assert_eq!(state.apps.get(&app_id).unwrap().image, spec.image);
    }

    #[tokio::test]
    async fn state_machine_snapshot_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sm.redb");
        let app_id = AppId::new("web", "prod");
        let spec = AppSpec {
            image: Some("persisted:v1".to_string()),
            ..default_spec()
        };

        // First run: apply an entry, then snapshot (persists to redb).
        {
            let db = std::sync::Arc::new(Database::create(&path).unwrap());
            let mut sm = CouncilStateMachine::with_store(db).unwrap();
            sm.apply(vec![normal_entry(
                1,
                1,
                RaftRequest::AppSpec {
                    app_id: app_id.clone(),
                    spec: Box::new(spec.clone()),
                },
            )])
            .await
            .unwrap();
            let mut builder = sm.get_snapshot_builder().await;
            builder.build_snapshot().await.unwrap();
        }

        // Reopen: the applied state is restored from the persisted snapshot.
        let db = std::sync::Arc::new(Database::create(&path).unwrap());
        let mut sm = CouncilStateMachine::with_store(db).unwrap();
        let state = sm.desired_state().await;
        assert_eq!(state.apps.get(&app_id).unwrap().image, spec.image);
        let (last_applied, _) = sm.applied_state().await.unwrap();
        assert_eq!(last_applied.map(|l| l.index), Some(1));
    }

    #[tokio::test]
    async fn with_store_on_an_absent_snapshot_loads_empty_state() {
        // No snapshot written: a fresh store opens with the default state.
        let dir = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(Database::create(dir.path().join("fresh.redb")).unwrap());
        let sm = CouncilStateMachine::with_store(db).expect("fresh store opens");
        assert!(sm.desired_state().await.apps.is_empty());
    }

    #[test]
    fn with_store_fails_closed_on_a_corrupt_snapshot() {
        // CP3: a snapshot blob that exists but won't decode must abort startup,
        // not silently boot an empty desired/security state.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.redb");
        {
            let db = Database::create(&path).unwrap();
            let wtx = db.begin_write().unwrap();
            {
                let mut t = wtx.open_table(SNAPSHOT).unwrap();
                t.insert(
                    SNAP_DATA_KEY,
                    b"this is not valid DesiredState json".as_slice(),
                )
                .unwrap();
                t.insert(SNAP_INDEX_KEY, 7u64.to_le_bytes().as_slice())
                    .unwrap();
            }
            wtx.commit().unwrap();
        }

        let db = std::sync::Arc::new(Database::create(&path).unwrap());
        let result = CouncilStateMachine::with_store(db);
        assert!(
            result.is_err(),
            "a present-but-corrupt snapshot must fail closed, not load empty"
        );
    }

    #[tokio::test]
    async fn apply_app_delete_removes_from_state() {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "prod");

        // Add then delete.
        let add = normal_entry(
            1,
            1,
            RaftRequest::AppSpec {
                app_id: app_id.clone(),
                spec: Box::new(default_spec()),
            },
        );
        let del = normal_entry(
            1,
            2,
            RaftRequest::AppDelete {
                app_id: app_id.clone(),
            },
        );
        sm.apply(vec![add, del]).await.unwrap();

        let state = sm.desired_state().await;
        assert!(state.apps.is_empty());
    }

    #[tokio::test]
    async fn apply_scheduling_decision_updates_placements() {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "prod");
        let decision = SchedulingDecision {
            app_id: app_id.clone(),
            placements: vec![
                Placement {
                    node_id: NodeId::new("node-1"),
                    resources: Resources::new(500, 256 * 1024 * 1024, 0),
                },
                Placement {
                    node_id: NodeId::new("node-2"),
                    resources: Resources::new(500, 256 * 1024 * 1024, 0),
                },
            ],
        };
        let entry = normal_entry(1, 1, RaftRequest::SchedulingDecision(decision));
        sm.apply(vec![entry]).await.unwrap();

        let state = sm.desired_state().await;
        let placements = state.scheduling.get(&app_id).unwrap();
        assert_eq!(placements.len(), 2);
    }

    #[tokio::test]
    async fn apply_config_set_updates_config() {
        let mut sm = CouncilStateMachine::new();
        let entry = normal_entry(
            1,
            1,
            RaftRequest::ConfigSet {
                key: "max_apps".to_string(),
                value: "100".to_string(),
            },
        );
        sm.apply(vec![entry]).await.unwrap();

        let state = sm.desired_state().await;
        assert_eq!(state.config.get("max_apps").unwrap(), "100");
    }

    #[tokio::test]
    async fn apply_noop_changes_nothing() {
        let mut sm = CouncilStateMachine::new();
        let entry = normal_entry(1, 1, RaftRequest::Noop);
        let responses = sm.apply(vec![entry]).await.unwrap();
        assert_eq!(responses.len(), 1);

        let state = sm.desired_state().await;
        assert!(state.apps.is_empty());
        assert!(state.scheduling.is_empty());
        assert!(state.config.is_empty());
    }

    #[tokio::test]
    async fn applied_state_returns_last_applied() {
        let mut sm = CouncilStateMachine::new();

        let (last_applied, _) = sm.applied_state().await.unwrap();
        assert!(last_applied.is_none());

        let entry = normal_entry(1, 5, RaftRequest::Noop);
        sm.apply(vec![entry]).await.unwrap();

        let (last_applied, _) = sm.applied_state().await.unwrap();
        assert_eq!(last_applied, Some(log_id(1, 5)));
    }

    #[tokio::test]
    async fn snapshot_round_trip() {
        let mut sm = CouncilStateMachine::new();

        // Apply some state.
        let entries = vec![
            normal_entry(
                1,
                1,
                RaftRequest::AppSpec {
                    app_id: AppId::new("web", "prod"),
                    spec: Box::new(default_spec()),
                },
            ),
            normal_entry(
                1,
                2,
                RaftRequest::ConfigSet {
                    key: "region".to_string(),
                    value: "us-east".to_string(),
                },
            ),
        ];
        sm.apply(entries).await.unwrap();

        // Build snapshot.
        let mut builder = sm.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.unwrap();
        assert_eq!(snapshot.meta.last_log_id, Some(log_id(1, 2)));

        // Deserialise the snapshot data and verify.
        let mut data = Vec::new();
        let mut cursor = *snapshot.snapshot;
        cursor.read_to_end(&mut data).unwrap();
        let restored: DesiredState = serde_json::from_slice(&data).unwrap();
        assert!(restored.apps.contains_key(&AppId::new("web", "prod")));
        assert_eq!(restored.config.get("region").unwrap(), "us-east");
    }

    #[tokio::test]
    async fn apply_membership_entry_updates_membership() {
        let mut sm = CouncilStateMachine::new();

        let membership = Membership::new(
            vec![std::collections::BTreeSet::from([1, 2, 3])],
            None::<std::collections::BTreeSet<u64>>,
        );
        let entry = openraft::Entry {
            log_id: log_id(1, 1),
            payload: EntryPayload::Membership(membership.clone()),
        };

        let responses = sm.apply(vec![entry]).await.unwrap();
        assert_eq!(responses.len(), 1);
        assert!(matches!(
            responses[0],
            CouncilResponse::Applied { log_index: 1 }
        ));

        let (last_applied, stored_membership) = sm.applied_state().await.unwrap();
        assert_eq!(last_applied, Some(log_id(1, 1)));
        assert_eq!(
            stored_membership.membership().get_joint_config().len(),
            membership.get_joint_config().len()
        );
    }

    #[tokio::test]
    async fn get_current_snapshot_returns_none_initially() {
        let mut sm = CouncilStateMachine::new();
        assert!(sm.get_current_snapshot().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn install_snapshot_replaces_state() {
        let mut sm = CouncilStateMachine::new();

        // Apply initial state.
        let entry = normal_entry(
            1,
            1,
            RaftRequest::AppSpec {
                app_id: AppId::new("old", "default"),
                spec: Box::new(default_spec()),
            },
        );
        sm.apply(vec![entry]).await.unwrap();

        // Build a new DesiredState to install.
        let mut new_state = DesiredState::default();
        new_state
            .apps
            .insert(AppId::new("new", "prod"), default_spec());
        new_state
            .config
            .insert("installed".to_string(), "true".to_string());
        let data = serde_json::to_vec(&new_state).unwrap();

        let meta = SnapshotMeta {
            last_log_id: Some(log_id(2, 10)),
            last_membership: StoredMembership::new(
                None,
                Membership::new(vec![], None::<std::collections::BTreeSet<u64>>),
            ),
            snapshot_id: "test-snap".to_string(),
        };

        sm.install_snapshot(&meta, Box::new(Cursor::new(data)))
            .await
            .unwrap();

        let state = sm.desired_state().await;
        // Old state gone, new state present.
        assert!(!state.apps.contains_key(&AppId::new("old", "default")));
        assert!(state.apps.contains_key(&AppId::new("new", "prod")));
        assert_eq!(state.config.get("installed").unwrap(), "true");
        assert_eq!(state.last_applied_log, Some(log_id(2, 10)));
    }

    // -- Pickle state machine tests ------------------------------------------

    fn test_digest(suffix: &str) -> crate::pickle::types::Digest {
        crate::pickle::types::Digest(format!("sha256:{suffix:0>64}"))
    }

    fn test_manifest_commit() -> crate::pickle::types::ManifestCommit {
        crate::pickle::types::ManifestCommit {
            manifest: crate::pickle::types::ImageManifest {
                digest: test_digest("m1"),
                config: crate::pickle::types::LayerDescriptor {
                    digest: test_digest("cfg"),
                    size: 512,
                    media_type: String::new(),
                },
                layers: vec![crate::pickle::types::LayerDescriptor {
                    digest: test_digest("layer1"),
                    size: 4096,
                    media_type: String::new(),
                }],
                repository: "myapp".to_string(),
                tags: std::collections::BTreeSet::new(),
                total_size: 4608,
                pushed_at: std::time::SystemTime::UNIX_EPOCH,
                pushed_by: 1,
                signature: None,
            },
            tag: "latest".to_string(),
            holder_nodes: std::collections::BTreeSet::from([1, 2]),
        }
    }

    #[tokio::test]
    async fn apply_manifest_commit_updates_catalog() {
        let mut sm = CouncilStateMachine::new();
        let commit = test_manifest_commit();
        let entry = normal_entry(1, 1, RaftRequest::ManifestCommit(commit));

        sm.apply(vec![entry]).await.unwrap();

        let state = sm.desired_state().await;
        let found = state
            .manifest_catalog
            .get_manifest_by_tag("myapp", "latest");
        assert!(found.is_some());
        assert_eq!(found.unwrap().repository, "myapp");
    }

    #[tokio::test]
    async fn apply_update_layer_locations() {
        let mut sm = CouncilStateMachine::new();
        let digest = test_digest("layer1");
        let update = crate::pickle::types::UpdateLayerLocations {
            updates: vec![(digest.clone(), std::collections::BTreeSet::from([3, 4]))],
        };
        let entry = normal_entry(1, 1, RaftRequest::UpdateLayerLocations(update));

        sm.apply(vec![entry]).await.unwrap();

        let state = sm.desired_state().await;
        let holders = state.manifest_catalog.layer_holders(digest.as_str());
        assert_eq!(holders, std::collections::BTreeSet::from([3, 4]));
    }

    #[tokio::test]
    async fn apply_gc_report_removes_holder() {
        let mut sm = CouncilStateMachine::new();

        // First: set up layer locations
        let digest = test_digest("layer1");
        let update = crate::pickle::types::UpdateLayerLocations {
            updates: vec![(digest.clone(), std::collections::BTreeSet::from([1, 2, 3]))],
        };
        sm.apply(vec![normal_entry(
            1,
            1,
            RaftRequest::UpdateLayerLocations(update),
        )])
        .await
        .unwrap();

        // Then: GC report removes node 2
        let report = crate::pickle::types::GcReport {
            node_id: 2,
            deleted_layers: vec![digest.clone()],
        };
        let responses = sm
            .apply(vec![normal_entry(1, 2, RaftRequest::GcReport(report))])
            .await
            .unwrap();

        // The deletion is safe (two holders remain), so it's approved.
        assert_eq!(
            responses[0],
            CouncilResponse::GcApproved {
                approved: vec![digest.clone()]
            }
        );
        let state = sm.desired_state().await;
        let holders = state.manifest_catalog.layer_holders(digest.as_str());
        assert_eq!(holders, std::collections::BTreeSet::from([1, 3]));
    }

    /// M2 regression: two nodes each holding one of two copies race to
    /// GC the same layer. The log serialises their reports; the first
    /// is approved, the second must be refused or the layer is lost.
    #[tokio::test]
    async fn gc_never_deletes_the_last_copy() {
        let mut sm = CouncilStateMachine::new();

        let digest = test_digest("precious");
        let update = crate::pickle::types::UpdateLayerLocations {
            updates: vec![(digest.clone(), std::collections::BTreeSet::from([1, 2]))],
        };
        sm.apply(vec![normal_entry(
            1,
            1,
            RaftRequest::UpdateLayerLocations(update),
        )])
        .await
        .unwrap();

        // Both nodes nominate the layer, in log order.
        let report_from_1 = crate::pickle::types::GcReport {
            node_id: 1,
            deleted_layers: vec![digest.clone()],
        };
        let report_from_2 = crate::pickle::types::GcReport {
            node_id: 2,
            deleted_layers: vec![digest.clone()],
        };
        let responses = sm
            .apply(vec![
                normal_entry(1, 2, RaftRequest::GcReport(report_from_1)),
                normal_entry(1, 3, RaftRequest::GcReport(report_from_2)),
            ])
            .await
            .unwrap();

        // Node 1 wins the race; node 2's nomination is refused.
        assert_eq!(
            responses[0],
            CouncilResponse::GcApproved {
                approved: vec![digest.clone()]
            }
        );
        assert_eq!(
            responses[1],
            CouncilResponse::GcApproved { approved: vec![] }
        );

        // The sole remaining copy (node 2's) is still tracked.
        let state = sm.desired_state().await;
        let holders = state.manifest_catalog.layer_holders(digest.as_str());
        assert_eq!(holders, std::collections::BTreeSet::from([2]));
    }

    #[tokio::test]
    async fn apply_delete_tag_removes_manifest() {
        let mut sm = CouncilStateMachine::new();

        // Push a manifest with tag "latest"
        let commit = test_manifest_commit();
        sm.apply(vec![normal_entry(
            1,
            1,
            RaftRequest::ManifestCommit(commit),
        )])
        .await
        .unwrap();

        // Delete the tag
        let delete = crate::pickle::types::DeleteTag {
            repository: "myapp".to_string(),
            tag: "latest".to_string(),
        };
        sm.apply(vec![normal_entry(1, 2, RaftRequest::DeleteTag(delete))])
            .await
            .unwrap();

        let state = sm.desired_state().await;
        assert!(
            state
                .manifest_catalog
                .get_manifest_by_tag("myapp", "latest")
                .is_none()
        );
    }

    // -- Deploy state machine tests ------------------------------------------

    fn test_deploy_state() -> crate::meat::deploy_types::DeployState {
        use crate::meat::deploy_types::*;
        DeployState::new(
            DeployId(1),
            DeployRequest {
                app_id: AppId::new("web", "prod"),
                new_image: "myapp:v2".to_string(),
                previous_image: Some("myapp:v1".to_string()),
                config: DeployConfig::default(),
                pre_deploy_jobs: Vec::new(),
            },
        )
    }

    fn test_deploy_history_entry() -> crate::meat::deploy_types::DeployHistoryEntry {
        use crate::meat::deploy_types::*;
        DeployHistoryEntry {
            id: DeployId(1),
            app_id: AppId::new("web", "prod"),
            image: "myapp:v2".to_string(),
            result: DeployResult::Completed,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            completed_at: std::time::SystemTime::UNIX_EPOCH,
            steps_completed: 3,
            steps_total: 3,
            spec: None,
        }
    }

    #[tokio::test]
    async fn apply_deploy_update_stores_active_deploy() {
        let mut sm = CouncilStateMachine::new();
        let deploy = test_deploy_state();
        let entry = normal_entry(
            1,
            1,
            RaftRequest::DeployUpdate {
                app_id: AppId::new("web", "prod"),
                state: Box::new(deploy),
            },
        );
        sm.apply(vec![entry]).await.unwrap();

        let state = sm.desired_state().await;
        assert_eq!(state.active_deploys.len(), 1);
    }

    #[tokio::test]
    async fn apply_deploy_complete_moves_to_history() {
        let mut sm = CouncilStateMachine::new();

        // First: start a deploy
        let deploy = test_deploy_state();
        sm.apply(vec![normal_entry(
            1,
            1,
            RaftRequest::DeployUpdate {
                app_id: AppId::new("web", "prod"),
                state: Box::new(deploy),
            },
        )])
        .await
        .unwrap();

        // Then: complete it
        let entry = test_deploy_history_entry();
        sm.apply(vec![normal_entry(
            1,
            2,
            RaftRequest::DeployComplete {
                app_id: AppId::new("web", "prod"),
                entry,
            },
        )])
        .await
        .unwrap();

        let state = sm.desired_state().await;
        assert!(state.active_deploys.is_empty());
        assert_eq!(state.deploy_history.len(), 1);
        assert_eq!(state.deploy_history[0].1.len(), 1);
    }

    #[tokio::test]
    async fn deploy_history_capped_at_50() {
        let mut sm = CouncilStateMachine::new();

        for i in 0..55 {
            let mut entry = test_deploy_history_entry();
            entry.id = crate::meat::deploy_types::DeployId(i);
            sm.apply(vec![normal_entry(
                1,
                i + 1,
                RaftRequest::DeployComplete {
                    app_id: AppId::new("web", "prod"),
                    entry,
                },
            )])
            .await
            .unwrap();
        }

        let state = sm.desired_state().await;
        let history = &state.deploy_history[0].1;
        assert_eq!(history.len(), 50);
    }

    #[tokio::test]
    async fn deploy_raft_serde_round_trip() {
        let deploy = test_deploy_state();
        let req = RaftRequest::DeployUpdate {
            app_id: AppId::new("web", "prod"),
            state: Box::new(deploy),
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: RaftRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, decoded);
    }

    // --- SecurityState Raft tests ---

    #[test]
    fn apply_security_state_init_sets_cas() {
        let mut inner = StateMachineInner::default();
        let ss = crate::sesame::types::SecurityState {
            next_serial: 42,
            ..Default::default()
        };
        inner.apply_request(&RaftRequest::SecurityStateInit(Box::new(ss)));

        assert_eq!(inner.state.security_state.next_serial, 42);
    }

    fn test_age_keypair(
        scope: crate::sesame::types::AgeKeyScope,
        generation: u64,
        read_only: bool,
    ) -> crate::sesame::types::AgeKeypair {
        crate::sesame::types::AgeKeypair {
            scope,
            public_key: format!("pub-{generation}"),
            private_key_wrapped: crate::sesame::types::WrappedKey {
                ciphertext: Vec::new(),
                nonce: [0u8; 12],
                hkdf_salt: [0u8; 32],
                hkdf_info: "test".to_string(),
            },
            generation,
            read_only,
        }
    }

    #[test]
    fn finalize_secret_rotation_retires_old_keys_once_a_replacement_exists() {
        use crate::sesame::types::AgeKeyScope;
        let mut inner = StateMachineInner::default();
        // The rotation flow: mark gen 0 read-only, add the active gen 1.
        inner
            .state
            .security_state
            .age_keypairs
            .push(test_age_keypair(AgeKeyScope::ClusterWide, 0, true));
        inner
            .state
            .security_state
            .age_keypairs
            .push(test_age_keypair(AgeKeyScope::ClusterWide, 1, false));

        inner.apply_request(&RaftRequest::FinalizeSecretRotation {
            scope: AgeKeyScope::ClusterWide,
        });

        let remaining = &inner.state.security_state.age_keypairs;
        assert_eq!(remaining.len(), 1, "the retiring gen 0 key is dropped");
        assert_eq!(remaining[0].generation, 1);
        assert!(!remaining[0].read_only);
    }

    #[test]
    fn finalize_secret_rotation_keeps_keys_when_no_active_replacement() {
        use crate::sesame::types::AgeKeyScope;
        let mut inner = StateMachineInner::default();
        // A stray finalize with only read-only keys must NOT wipe the scope —
        // that would make every secret sealed under it undecryptable (PKI8).
        inner
            .state
            .security_state
            .age_keypairs
            .push(test_age_keypair(AgeKeyScope::ClusterWide, 0, true));

        inner.apply_request(&RaftRequest::FinalizeSecretRotation {
            scope: AgeKeyScope::ClusterWide,
        });

        assert_eq!(
            inner.state.security_state.age_keypairs.len(),
            1,
            "no active key means nothing is retired"
        );
    }

    #[test]
    fn apply_create_join_token() {
        let mut inner = StateMachineInner::default();
        let jt = crate::sesame::types::JoinToken {
            token_hash: [0xAB; 32],
            expires_at: std::time::SystemTime::now(),
            consumed: false,
            attestation_mode: crate::sesame::types::AttestationMode::None,
        };
        inner.apply_request(&RaftRequest::CreateJoinToken(jt));
        assert_eq!(inner.state.security_state.join_tokens.len(), 1);
        assert!(!inner.state.security_state.join_tokens[0].consumed);
    }

    #[test]
    fn apply_consume_join_token() {
        let mut inner = StateMachineInner::default();
        let jt = crate::sesame::types::JoinToken {
            token_hash: [0xAB; 32],
            expires_at: std::time::SystemTime::now(),
            consumed: false,
            attestation_mode: crate::sesame::types::AttestationMode::None,
        };
        inner.apply_request(&RaftRequest::CreateJoinToken(jt));
        inner.apply_request(&RaftRequest::ConsumeJoinToken {
            token_hash: [0xAB; 32],
        });
        assert!(inner.state.security_state.join_tokens[0].consumed);
    }

    #[test]
    fn apply_create_api_token() {
        let mut inner = StateMachineInner::default();
        let token = crate::sesame::types::ApiToken {
            name: "ci".to_string(),
            token_hash: vec![1, 2, 3],
            token_salt: vec![4, 5, 6],
            role: crate::sesame::types::ApiRole::Deployer,
            scope: crate::sesame::types::TokenScope::default(),
            expires_at: None,
            created_at: std::time::SystemTime::now(),
        };
        inner.apply_request(&RaftRequest::CreateApiToken(token));
        assert_eq!(inner.state.security_state.api_tokens.len(), 1);
        assert_eq!(inner.state.security_state.api_tokens[0].name, "ci");
    }

    #[test]
    fn apply_revoke_api_token() {
        let mut inner = StateMachineInner::default();
        let token = crate::sesame::types::ApiToken {
            name: "ci".to_string(),
            token_hash: vec![1, 2, 3],
            token_salt: vec![4, 5, 6],
            role: crate::sesame::types::ApiRole::Deployer,
            scope: crate::sesame::types::TokenScope::default(),
            expires_at: None,
            created_at: std::time::SystemTime::now(),
        };
        inner.apply_request(&RaftRequest::CreateApiToken(token));
        assert_eq!(inner.state.security_state.api_tokens.len(), 1);

        inner.apply_request(&RaftRequest::RevokeApiToken {
            name: "ci".to_string(),
        });
        assert!(inner.state.security_state.api_tokens.is_empty());
    }

    #[test]
    fn apply_allocate_serial_increments() {
        let mut inner = StateMachineInner::default();
        assert_eq!(inner.state.security_state.next_serial, 0);

        // Each apply returns the distinct serial it allocated — so two callers
        // never derive the same value.
        let first = inner.apply_request(&RaftRequest::AllocateSerial);
        assert_eq!(first, Some(CouncilResponse::SerialAllocated { serial: 0 }));
        assert_eq!(inner.state.security_state.next_serial, 1);

        let second = inner.apply_request(&RaftRequest::AllocateSerial);
        assert_eq!(second, Some(CouncilResponse::SerialAllocated { serial: 1 }));
        assert_eq!(inner.state.security_state.next_serial, 2);

        assert_ne!(first, second);

        // Requests without a bespoke response return None.
        assert_eq!(inner.apply_request(&RaftRequest::Noop), None);
    }

    // ---- cluster upgrade state (Phase 14) ----

    fn upgrade_state(upgrade_id: &str) -> crate::upgrade::types::ClusterUpgradeState {
        crate::upgrade::types::ClusterUpgradeState {
            upgrade_id: upgrade_id.to_string(),
            target_version: "v0.2.0".parse().unwrap(),
            binary_sha256: "abc123".to_string(),
            embedded_signature: "sig".to_string(),
            external_signature: None,
            parallel: 1,
            direction: crate::upgrade::types::UpgradeDirection::Upgrade,
            phase: crate::upgrade::types::ClusterUpgradePhase::Preparing,
            registry_address: String::new(),
            nodes: vec![],
        }
    }

    #[test]
    fn upgrade_update_replaces_active_state() {
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::UpgradeUpdate {
            state: Box::new(upgrade_state("up-1")),
        });
        let mut advanced = upgrade_state("up-1");
        advanced.phase = crate::upgrade::types::ClusterUpgradePhase::UpgradingWorkers;
        inner.apply_request(&RaftRequest::UpgradeUpdate {
            state: Box::new(advanced.clone()),
        });

        assert_eq!(inner.state.active_upgrade, Some(advanced));
        assert!(inner.state.upgrade_history.is_empty());
    }

    #[test]
    fn upgrade_clear_archives_to_history() {
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::UpgradeUpdate {
            state: Box::new(upgrade_state("up-1")),
        });

        // A clear for a DIFFERENT id must not touch the active upgrade.
        inner.apply_request(&RaftRequest::UpgradeClear {
            upgrade_id: "up-other".to_string(),
        });
        assert!(inner.state.active_upgrade.is_some());

        inner.apply_request(&RaftRequest::UpgradeClear {
            upgrade_id: "up-1".to_string(),
        });
        assert!(inner.state.active_upgrade.is_none());
        assert_eq!(inner.state.upgrade_history.len(), 1);
        assert_eq!(inner.state.upgrade_history[0].upgrade_id, "up-1");
    }

    #[test]
    fn upgrade_history_is_bounded() {
        let mut inner = StateMachineInner::default();
        for i in 0..25 {
            let id = format!("up-{i}");
            inner.apply_request(&RaftRequest::UpgradeUpdate {
                state: Box::new(upgrade_state(&id)),
            });
            inner.apply_request(&RaftRequest::UpgradeClear { upgrade_id: id });
        }
        assert_eq!(inner.state.upgrade_history.len(), 20);
        assert_eq!(inner.state.upgrade_history[0].upgrade_id, "up-5");
    }

    #[test]
    fn old_snapshot_without_upgrade_fields_still_loads() {
        // Serialise a current DesiredState, strip the Phase 14 fields to
        // fake a snapshot written by an older binary, and reload. This is
        // the serde(default) compatibility rule made executable.
        let state = DesiredState::default();
        let mut value = serde_json::to_value(&state).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("active_upgrade");
        object.remove("upgrade_history");

        let reloaded: DesiredState = serde_json::from_value(value).unwrap();
        assert!(reloaded.active_upgrade.is_none());
        assert!(reloaded.upgrade_history.is_empty());
    }

    #[test]
    fn snapshot_with_active_upgrade_roundtrips() {
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::UpgradeUpdate {
            state: Box::new(upgrade_state("up-1")),
        });

        let json = serde_json::to_string(&inner.state).unwrap();
        let reloaded: DesiredState = serde_json::from_str(&json).unwrap();
        assert_eq!(reloaded.active_upgrade, inner.state.active_upgrade);
    }
}
