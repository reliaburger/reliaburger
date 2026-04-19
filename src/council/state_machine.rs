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
use tokio::sync::RwLock;

use super::types::{CouncilNodeInfo, CouncilResponse, DesiredState, RaftRequest, TypeConfig};

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct StateMachineInner {
    state: DesiredState,
    snapshot_index: u64,
    snapshot_data: Option<Vec<u8>>,
}

impl StateMachineInner {
    fn apply_request(&mut self, request: &RaftRequest) {
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
                self.state.manifest_catalog.apply_gc_report(report);
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
                self.state.security_state.next_serial += 1;
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
                // Remove read-only keypairs with the same scope
                self.state
                    .security_state
                    .age_keypairs
                    .retain(|kp| kp.scope != *scope || !kp.read_only);
            }
            RaftRequest::RevokeCertificate(entry) => {
                self.state.security_state.crl.entries.push(entry.clone());
                self.state.security_state.crl.version += 1;
                self.state.security_state.crl.updated_at = std::time::SystemTime::now();
            }
            RaftRequest::Noop => {}
        }
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
    /// Create a new empty state machine.
    pub fn new() -> Self {
        Self::default()
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
                    guard.apply_request(&request);
                    responses.push(CouncilResponse::Applied {
                        log_index: log_id.index,
                    });
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
        guard.snapshot_data = Some(data);
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
        sm.apply(vec![normal_entry(1, 2, RaftRequest::GcReport(report))])
            .await
            .unwrap();

        let state = sm.desired_state().await;
        let holders = state.manifest_catalog.layer_holders(digest.as_str());
        assert_eq!(holders, std::collections::BTreeSet::from([1, 3]));
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
        let mut ss = crate::sesame::types::SecurityState::default();
        ss.next_serial = 42;
        inner.apply_request(&RaftRequest::SecurityStateInit(Box::new(ss)));

        assert_eq!(inner.state.security_state.next_serial, 42);
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

        inner.apply_request(&RaftRequest::AllocateSerial);
        assert_eq!(inner.state.security_state.next_serial, 1);

        inner.apply_request(&RaftRequest::AllocateSerial);
        assert_eq!(inner.state.security_state.next_serial, 2);
    }
}
