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
use redb::{Database, ReadableTable, TableDefinition};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use super::types::{CouncilNodeInfo, CouncilResponse, DesiredState, RaftRequest, TypeConfig};
use crate::sesame::types::AgeKeyScope;

/// Persisted snapshot: `data` = JSON of `DesiredState` (which itself carries
/// `last_applied_log` + `last_membership`), `index` = snapshot counter,
/// `version` = on-disk format version, `checksum` = SHA-256 of `data`.
/// All four keys are written in one transaction, so they are always coherent.
const SNAPSHOT: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_snapshot");
const SNAP_DATA_KEY: &str = "data";
const SNAP_INDEX_KEY: &str = "index";
const SNAP_VERSION_KEY: &str = "version";
const SNAP_CHECKSUM_KEY: &str = "checksum";

/// Snapshot format version this binary writes. Bump it when the persisted
/// layout changes incompatibly; loading rejects versions it doesn't know.
const SNAPSHOT_FORMAT_VERSION: u32 = crate::compatibility::CURRENT.state;

/// Errors opening or validating the persisted snapshot store.
///
/// Every variant is startup-fatal: after log compaction the snapshot is the
/// only copy of the covered log prefix, so a snapshot that cannot be trusted
/// must refuse startup instead of booting an empty cluster state (CP3).
#[derive(Debug, thiserror::Error)]
pub enum SnapshotStoreError {
    #[error(transparent)]
    Transaction(#[from] redb::TransactionError),
    #[error(transparent)]
    Table(#[from] redb::TableError),
    #[error(transparent)]
    Storage(#[from] redb::StorageError),
    #[error(transparent)]
    Commit(#[from] redb::CommitError),
    #[error("snapshot payload failed checksum verification: stored {stored}, computed {computed}")]
    ChecksumMismatch { stored: String, computed: String },
    #[error("snapshot records format version {version} but no checksum")]
    MissingChecksum { version: u32 },
    #[error("snapshot format version {found} is not supported (this binary requires {supported})")]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error("snapshot version marker is malformed: expected 4 bytes, found {found}")]
    MalformedVersion { found: usize },
    #[error("snapshot present but failed to decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error(
        "raft log purged up to index {purged_index} but no snapshot exists to cover it; compacted state cannot be reconstructed"
    )]
    PurgedWithoutSnapshot { purged_index: u64 },
    #[error(
        "raft log purged up to index {purged_index} but the snapshot only covers up to index {snapshot_index}; compacted state cannot be reconstructed"
    )]
    PurgedBeyondSnapshot {
        purged_index: u64,
        snapshot_index: u64,
    },
    #[error("raft log store read failed: {0}")]
    LogRead(#[from] StorageError<u64>),
}

#[cfg(test)]
thread_local! {
    // Observe real desired-state writes on this test's current-thread runtime.
    static PREREQUISITE_NAMESPACE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

/// The entry being applied and the admission revision immediately before it.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApplyEntryPosition {
    /// Shared planning revision, captured before advancing the applied log.
    pub previous_log_id: Option<LogId<u64>>,
    /// Current entry; leader-term guards continue to use this position.
    pub current_log_id: Option<LogId<u64>>,
}

#[derive(Default)]
struct StateMachineInner {
    state: DesiredState,
    snapshot_index: u64,
    snapshot_data: Option<Vec<u8>>,
    /// The log id and membership `snapshot_data` covers. The live `state`
    /// moves on as entries apply; a snapshot handed to a learner must
    /// describe its own contents, or the learner records a log position its
    /// state doesn't hold.
    snapshot_last_log_id: Option<LogId<u64>>,
    snapshot_membership: StoredMembership<u64, CouncilNodeInfo>,
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

/// Write the latest snapshot (data + index + version + checksum) to redb,
/// fsyncing on commit. The version and checksum keys land in the same write
/// transaction as the payload they describe.
// `redb::Error` is large but dictated by the crate; boxing it here buys nothing.
#[allow(clippy::result_large_err)]
fn persist_snapshot(db: &Database, data: &[u8], index: u64) -> Result<(), redb::Error> {
    let checksum = snapshot_checksum(data);
    let wtx = db.begin_write()?;
    {
        let mut t = wtx.open_table(SNAPSHOT)?;
        t.insert(SNAP_DATA_KEY, data)?;
        t.insert(SNAP_INDEX_KEY, index.to_le_bytes().as_slice())?;
        t.insert(
            SNAP_VERSION_KEY,
            SNAPSHOT_FORMAT_VERSION.to_le_bytes().as_slice(),
        )?;
        t.insert(SNAP_CHECKSUM_KEY, checksum.as_slice())?;
    }
    wtx.commit()?;
    Ok(())
}

/// Persist a snapshot on the blocking pool (M7). `persist_snapshot` commits to
/// redb, which fsyncs — running it inline on an async openraft method blocked a
/// tokio runtime worker on disk I/O. A panic in the task maps to a write error.
// `StorageError<u64>` is large but dictated by openraft; boxing it buys nothing.
#[allow(clippy::result_large_err)]
async fn persist_snapshot_blocking(
    db: Arc<Database>,
    data: Vec<u8>,
    index: u64,
) -> Result<(), StorageError<u64>> {
    match tokio::task::spawn_blocking(move || persist_snapshot(&db, &data, index)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(StorageError::from(StorageIOError::write_state_machine(&e))),
        Err(e) => Err(StorageError::from(StorageIOError::write_state_machine(&e))),
    }
}

/// SHA-256 of the snapshot payload bytes.
fn snapshot_checksum(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Read the stored format version, `None` when the store has no version key.
// `SnapshotStoreError` is large but dictated by the errors it wraps.
#[allow(clippy::result_large_err)]
fn read_snapshot_version(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<Option<u32>, SnapshotStoreError> {
    match table.get(SNAP_VERSION_KEY)? {
        Some(guard) => {
            let bytes = guard.value().to_vec();
            let bytes: [u8; 4] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| SnapshotStoreError::MalformedVersion { found: bytes.len() })?;
            Ok(Some(u32::from_le_bytes(bytes)))
        }
        None => Ok(None),
    }
}

/// Verify the stored checksum matches the payload bytes.
// `SnapshotStoreError` is large but dictated by the errors it wraps.
#[allow(clippy::result_large_err)]
fn verify_snapshot_checksum(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    payload: &[u8],
) -> Result<(), SnapshotStoreError> {
    let stored = table
        .get(SNAP_CHECKSUM_KEY)?
        .ok_or(SnapshotStoreError::MissingChecksum {
            version: SNAPSHOT_FORMAT_VERSION,
        })?
        .value()
        .to_vec();
    let computed = snapshot_checksum(payload);
    if stored != computed {
        return Err(SnapshotStoreError::ChecksumMismatch {
            stored: hex::encode(stored),
            computed: hex::encode(computed),
        });
    }
    Ok(())
}

/// Whether `keypair` is the age key `relish init` sealed the root CA backup
/// to: the cluster-wide key of generation 0. Nothing else seals to it, so
/// finalising a rotation keeps it, read-only.
fn opens_root_ca_backup(keypair: &crate::sesame::types::AgeKeypair) -> bool {
    keypair.scope == crate::sesame::types::AgeKeyScope::ClusterWide && keypair.generation == 0
}

impl StateMachineInner {
    fn registry_publication_is_current(
        &self,
        commit: &crate::pickle::types::ManifestCommit,
    ) -> bool {
        commit.holder_nodes.iter().all(|node| {
            self.state
                .registry_gc_generations
                .get(node)
                .copied()
                .unwrap_or(0)
                == commit.observed_gc_generation
        })
    }

    fn registry_node_retired(&self, node_id: u64) -> bool {
        self.state
            .security_state
            .crl
            .retired_nodes
            .keys()
            .any(|name| crate::cluster::identity::raft_id_from_name(name) == node_id)
    }

    fn lease_workloads_absent(&self, lease: &crate::testkit::lease::TestLease) -> bool {
        use crate::testkit::lease::{LeasedResource, TestLeaseState};
        matches!(lease.state, TestLeaseState::Cleaning { .. })
            && lease.placements.is_empty()
            && lease.resources.iter().all(|resource| match resource {
                LeasedResource::App { app_id } => !self.state.apps.contains_key(app_id),
                LeasedResource::Job { .. } => false,
                LeasedResource::Namespace { name } => !self.state.namespaces.contains_key(name),
                LeasedResource::ApiToken { name, .. } => !self
                    .state
                    .security_state
                    .api_tokens
                    .iter()
                    .any(|token| token.name == *name),
            })
    }

    fn apply_scheduling_decision(
        &mut self,
        decision: &crate::meat::SchedulingDecision,
    ) -> Option<CouncilResponse> {
        if decision.placements.iter().any(|placement| {
            self.state
                .security_state
                .crl
                .retired_nodes
                .contains_key(&placement.node_id.0)
        }) {
            return Some(CouncilResponse::Refused {
                reason: "placement targets a retired node identity".into(),
            });
        }
        // An instance is named after its ordinal, so two placements
        // sharing one would give two replicas the same id (#398).
        let mut ordinals = std::collections::HashSet::new();
        if !decision
            .placements
            .iter()
            .all(|placement| ordinals.insert(placement.ordinal))
        {
            return Some(CouncilResponse::Refused {
                reason: format!("two placements of {} share an ordinal", decision.app_id),
            });
        }

        if decision.app_id.namespace.starts_with("rbtest-") {
            let resource = crate::testkit::lease::LeasedResource::App {
                app_id: decision.app_id.clone(),
            };
            let Some(lease) = self
                .state
                .test_leases
                .values_mut()
                .find(|lease| lease.resources.contains(&resource))
            else {
                return Some(CouncilResponse::Refused {
                    reason: "leased scheduling requires an owner".into(),
                });
            };
            if !matches!(lease.state, crate::testkit::lease::TestLeaseState::Active)
                || !self.state.apps.contains_key(&decision.app_id)
            {
                return Some(CouncilResponse::Refused {
                    reason: "lease is cleaning or application was deleted".into(),
                });
            }
            let mut owners = lease.placements.clone();
            for placement in &decision.placements {
                if placement.node_id.0.is_empty() {
                    return Some(CouncilResponse::Refused {
                        reason: "placement node is empty".into(),
                    });
                }
                owners.insert(crate::testkit::lease::LeasedPlacement {
                    app_id: decision.app_id.clone(),
                    node_id: placement.node_id.clone(),
                });
            }
            if owners.len() > crate::testkit::lease::MAX_LEASED_PLACEMENTS {
                return Some(CouncilResponse::Refused {
                    reason: "lease placement history limit reached".into(),
                });
            }
            lease.placements = owners;
        }
        // A stop commits an empty decision; where the app ran must
        // outlive it, because that's where its managed volumes are.
        // Recorded in ordinal order, so a returning app gets each
        // home's ordinal back.
        if !decision.placements.is_empty() {
            let mut placed: Vec<_> = decision.placements.iter().collect();
            placed.sort_by_key(|placement| placement.ordinal);
            self.state.last_placed_nodes.insert(
                decision.app_id.clone(),
                placed
                    .into_iter()
                    .map(|placement| placement.node_id.clone())
                    .collect(),
            );
        }
        self.state
            .scheduling
            .insert(decision.app_id.clone(), decision.placements.clone());
        None
    }

    /// Apply a request. Returns a request-specific response for entries
    /// that carry a verdict back to the proposer (`AllocateSerial` gets
    /// its serial, `GcReport` gets the approved deletions); `None` means
    /// the generic `Applied` response.
    #[cfg(test)]
    fn apply_request(&mut self, request: &RaftRequest) -> Option<CouncilResponse> {
        let position = ApplyEntryPosition {
            previous_log_id: self.state.last_applied_log,
            current_log_id: self.state.last_applied_log,
        };
        let guarded;
        let request = if let RaftRequest::SchedulingDecision(decision) = request {
            guarded = RaftRequest::SchedulingDecisions {
                expected_log_id: position.previous_log_id,
                decisions: vec![decision.clone()],
            };
            &guarded
        } else {
            request
        };
        self.apply_request_at(request, position)
    }

    fn apply_request_at(
        &mut self,
        request: &RaftRequest,
        position: ApplyEntryPosition,
    ) -> Option<CouncilResponse> {
        match request {
            RaftRequest::PrerequisiteBegin {
                operation_id,
                term,
                config,
            } => {
                use super::prerequisites::{MAX_CLAIMS, PrerequisiteClaim};
                let refusal = |reason: String| Some(CouncilResponse::Refused { reason });
                if self
                    .state
                    .last_applied_log
                    .is_none_or(|log| log.leader_id.term != *term)
                {
                    return refusal("stale prerequisite leadership term".into());
                }
                let namespaces: Vec<_> = self.state.namespaces.keys().cloned().collect();
                if let Err(error) = config.validate_against(&namespaces) {
                    return refusal(error.to_string());
                }
                if self.state.prerequisite_claims.contains_key(operation_id)
                    || self.state.prerequisite_claims.len() >= MAX_CLAIMS
                {
                    return refusal(
                        "prerequisite ownership capacity exhausted or operation already exists"
                            .into(),
                    );
                }
                if let Some(owner) =
                    super::prerequisites::conflict(&self.state.prerequisite_claims, config)
                {
                    return refusal(format!(
                        "prerequisite operation {owner} still owns a workload"
                    ));
                }
                // Confirm finite ownership before cloning or staging desired-state writes.
                let mut next = self.state.prerequisite_claims.clone();
                next.insert(
                    operation_id.clone(),
                    PrerequisiteClaim {
                        term: *term,
                        recovery_epoch: self.state.recovery_epoch,
                        apps_committed: false,
                        config: *config.clone(),
                    },
                );
                if let Err(reason) = super::prerequisites::validate_claims(&next) {
                    return refusal(reason);
                }
                // Jobs launch under their canonical physical names on the local
                // node. Permanent global batch ownership outlives tracker pruning.
                if config.job.iter().any(|(name, job)| {
                    self.state
                        .batch_state
                        .execution_owner(job.namespace.as_deref().unwrap_or("default"), name)
                        .is_some()
                }) {
                    return refusal("job identity belongs to a batch execution".into());
                }
                // Reject known desired-state admission failures before authorizing
                // migration side effects. Commit repeats this against current state.
                let mut staged = StateMachineInner {
                    state: self.state.clone(),
                    ..Default::default()
                };
                for write in super::config_to_desired_writes(config) {
                    if let Some(CouncilResponse::Refused { reason }) =
                        staged.apply_request_at(&write, position)
                    {
                        return refusal(reason);
                    }
                }
                self.state.prerequisite_claims = next;
            }
            RaftRequest::PrerequisiteCommit { operation_id } => {
                let Some(claim) = self.state.prerequisite_claims.get(operation_id).cloned() else {
                    return Some(CouncilResponse::Refused {
                        reason: "unknown prerequisite operation".into(),
                    });
                };
                if self
                    .state
                    .last_applied_log
                    .is_none_or(|log| log.leader_id.term != claim.term)
                    || claim.recovery_epoch != self.state.recovery_epoch
                {
                    return Some(CouncilResponse::Refused { reason: "prerequisite worker belongs to an earlier leadership or recovery generation".into() });
                }
                if claim.apps_committed {
                    return Some(CouncilResponse::Refused {
                        reason: "prerequisite desired writes already committed".into(),
                    });
                }
                let namespaces: Vec<_> = self.state.namespaces.keys().cloned().collect();
                if let Err(error) = claim.config.validate_against(&namespaces) {
                    return Some(CouncilResponse::Refused {
                        reason: error.to_string(),
                    });
                }
                // Only the staged copy temporarily removes this operation's fence.
                // A refused desired write publishes nothing and retains ownership.
                let writes = super::config_to_desired_writes(&claim.config);
                let mut staged = StateMachineInner {
                    state: self.state.clone(),
                    ..Default::default()
                };
                staged.state.prerequisite_claims.remove(operation_id);
                for write in writes {
                    if let Some(CouncilResponse::Refused { reason }) =
                        staged.apply_request_at(&write, position)
                    {
                        return Some(CouncilResponse::Refused { reason });
                    }
                }
                if claim.has_ordinary_jobs() {
                    let mut retained = claim;
                    retained.apps_committed = true;
                    staged
                        .state
                        .prerequisite_claims
                        .insert(operation_id.clone(), retained);
                    if let Err(reason) =
                        super::prerequisites::validate_claims(&staged.state.prerequisite_claims)
                    {
                        return Some(CouncilResponse::Refused { reason });
                    }
                }
                self.state = staged.state;
            }
            RaftRequest::PrerequisiteFailed { operation_id } => {
                let Some(claim) = self.state.prerequisite_claims.get(operation_id) else {
                    return Some(CouncilResponse::Refused {
                        reason: "unknown prerequisite operation".into(),
                    });
                };
                if self
                    .state
                    .last_applied_log
                    .is_none_or(|log| log.leader_id.term != claim.term)
                    || claim.recovery_epoch != self.state.recovery_epoch
                {
                    return Some(CouncilResponse::Refused { reason: "cannot release uncertain prerequisite ownership from an earlier leadership or recovery generation".into() });
                }
                if claim.apps_committed {
                    return Some(CouncilResponse::Refused {
                        reason: "committed ordinary jobs require positive job settlement".into(),
                    });
                }
                self.state.prerequisite_claims.remove(operation_id);
            }
            RaftRequest::JobApplyComplete { operation_id } => {
                let Some(claim) = self.state.prerequisite_claims.get(operation_id) else {
                    return Some(CouncilResponse::Refused {
                        reason: "unknown job apply operation".into(),
                    });
                };
                if self
                    .state
                    .last_applied_log
                    .is_none_or(|log| log.leader_id.term != claim.term)
                    || claim.recovery_epoch != self.state.recovery_epoch
                    || !claim.apps_committed
                {
                    return Some(CouncilResponse::Refused { reason: "job completion does not own this committed leadership and recovery generation".into() });
                }
                self.state.prerequisite_claims.remove(operation_id);
            }
            RaftRequest::ReserveNodeFault {
                reservation,
                membership_log_id,
                unavailable_voters,
            } => {
                let membership = self.state.last_membership.membership();
                let voters: std::collections::BTreeSet<_> = membership.voter_ids().collect();
                let refuse = |reason: &str| {
                    Some(CouncilResponse::Refused {
                        reason: reason.into(),
                    })
                };
                if self.state.last_membership.log_id() != membership_log_id
                    || membership.get_joint_config().len() != 1
                    || voters.is_empty()
                {
                    return refuse(
                        "node fault safety requires a stable current council membership",
                    );
                }
                if reservation
                    .request
                    .target_node
                    .as_ref()
                    .is_some_and(|node| {
                        self.state
                            .security_state
                            .crl
                            .retired_nodes
                            .contains_key(node)
                    })
                {
                    return refuse("node identity is retired");
                }
                // Pressure may starve a voter just as effectively as a transport
                // fault. Drain alone only withdraws scheduler readiness.
                let quorum_effect = !matches!(
                    reservation.request.fault_type,
                    crate::smoker::types::FaultType::NodeDrain
                );
                if quorum_effect
                    && unavailable_voters.intersection(&voters).count() + 1 > (voters.len() - 1) / 2
                {
                    return refuse("node fault would risk council quorum");
                }
                if let Err(reason) = self.state.node_fault_reservations.reserve(reservation) {
                    return Some(CouncilResponse::Refused { reason });
                }
            }
            RaftRequest::ReleaseNodeFault { sequence } => {
                if let Err(reason) = self.state.node_fault_reservations.release(*sequence) {
                    return Some(CouncilResponse::Refused { reason });
                }
            }
            RaftRequest::AppSpec { app_id, spec } => {
                if self
                    .state
                    .batch_state
                    .execution_owner(&app_id.namespace, &app_id.name)
                    .is_some()
                {
                    return Some(CouncilResponse::Refused {
                        reason: "identity belongs to a batch execution".into(),
                    });
                }
                if self
                    .state
                    .prerequisite_claims
                    .values()
                    .any(|claim| claim.blocks(&app_id.name, &app_id.namespace))
                {
                    return Some(CouncilResponse::Refused {
                        reason: "an outstanding prerequisite owns this app".into(),
                    });
                }
                if crate::testkit::lease::valid_test_namespace(&app_id.namespace) {
                    return Some(CouncilResponse::Refused {
                        reason: "test lease namespace requires a leased app write".to_string(),
                    });
                }
                if let Err(error) =
                    crate::testkit::lease::authorise_image_references(spec.image_references(), None)
                {
                    return Some(CouncilResponse::Refused {
                        reason: error.to_string(),
                    });
                }
                self.apply_app_spec(app_id, spec);
            }
            RaftRequest::AppStop { app_id } => {
                if self
                    .state
                    .prerequisite_claims
                    .values()
                    .any(|claim| claim.blocks(&app_id.name, &app_id.namespace))
                {
                    return Some(CouncilResponse::Refused {
                        reason: "an outstanding job operation owns this app".into(),
                    });
                }
                if !self.state.apps.contains_key(app_id) {
                    return Some(CouncilResponse::Refused {
                        reason: format!("app {app_id} is not deployed"),
                    });
                }
                self.state.stopped_apps.insert(app_id.clone());
            }
            RaftRequest::AppDelete { app_id } => {
                if self
                    .state
                    .prerequisite_claims
                    .values()
                    .any(|claim| claim.blocks(&app_id.name, &app_id.namespace))
                {
                    return Some(CouncilResponse::Refused {
                        reason: "an outstanding job operation owns this app".into(),
                    });
                }
                self.state.apps.remove(app_id);
                self.state.stopped_apps.remove(app_id);
                self.state.scheduling.remove(app_id);
                self.state.last_placed_nodes.remove(app_id);
                self.state.quota_blocked.remove(app_id);
                // A deleted app leaves no baseline for an override to sit
                // above; drop it so a re-created app of the same name starts
                // from its own spec, not a ghost override (DEP8).
                let key = app_id.to_string();
                self.state.autoscale_overrides.retain(|(k, _)| k != &key);
                let prefix = format!("{app_id}/");
                self.state
                    .security_state
                    .secret_seals
                    .retain(|key, _| !key.starts_with(&prefix));
            }
            RaftRequest::SchedulingDecision(_) => {
                return Some(CouncilResponse::Refused {
                    reason: "placement requires a guarded whole-pass admission".into(),
                });
            }
            RaftRequest::SchedulingDecisions {
                expected_log_id,
                decisions,
            } => {
                if *expected_log_id != position.previous_log_id {
                    return Some(CouncilResponse::Refused {
                        reason: "admission revision changed".into(),
                    });
                }
                if decisions.len() > 4096
                    || decisions
                        .iter()
                        .map(|d| d.placements.len())
                        .try_fold(0usize, |n, size| n.checked_add(size))
                        .is_none_or(|n| n > 131072)
                {
                    return Some(CouncilResponse::Refused {
                        reason: "placement pass exceeds admission limits".into(),
                    });
                }
                let mut identities = std::collections::HashSet::new();
                let mut staged = StateMachineInner {
                    state: self.state.clone(),
                    ..Default::default()
                };
                for decision in decisions {
                    if !identities.insert(&decision.app_id) {
                        return Some(CouncilResponse::Refused {
                            reason: "placement pass repeats an application".into(),
                        });
                    }
                    if let Some(response) = staged.apply_scheduling_decision(decision) {
                        return Some(response);
                    }
                }
                self.state = staged.state;
            }
            RaftRequest::ConfigSet { key, value } => {
                self.state.config.insert(key.clone(), value.clone());
            }
            RaftRequest::ManifestCommit(commit) => {
                if !self.registry_publication_is_current(commit) {
                    return Some(CouncilResponse::RegistryPublicationStale);
                }
                if commit
                    .manifest
                    .repository
                    .split_once('/')
                    .is_some_and(|(namespace, _)| namespace.starts_with("rbtest-"))
                {
                    return Some(CouncilResponse::Refused {
                        reason: "test repositories require an active lease and writer receipt"
                            .into(),
                    });
                }
                if self
                    .state
                    .security_state
                    .crl
                    .retired_nodes
                    .keys()
                    .any(|name| {
                        let id = crate::cluster::identity::raft_id_from_name(name);
                        commit.manifest.pushed_by == id || commit.holder_nodes.contains(&id)
                    })
                {
                    return Some(CouncilResponse::Refused {
                        reason: "registry writer identity is retired".into(),
                    });
                }
                self.state.manifest_catalog.apply_manifest_commit(commit);
            }
            RaftRequest::UpdateLayerLocations(_) => {
                return Some(CouncilResponse::Refused {
                    reason: "holder replacement requires storage-node copy confirmation".into(),
                });
            }
            RaftRequest::ConfirmImageCopy(copy) => {
                if self.registry_node_retired(copy.node_id) {
                    return Some(CouncilResponse::Refused {
                        reason: "registry writer identity is retired".into(),
                    });
                }
                if self
                    .state
                    .registry_gc_generations
                    .get(&copy.node_id)
                    .copied()
                    .unwrap_or(0)
                    != copy.observed_gc_generation
                {
                    return Some(CouncilResponse::RegistryPublicationStale);
                }
                let catalogue = &self.state.manifest_catalog;
                let reserved = copy
                    .repository
                    .split_once('/')
                    .is_some_and(|(namespace, _)| namespace.starts_with("rbtest-"));
                let permitted = match &copy.lease_id {
                    Some(id) => {
                        reserved
                            && catalogue.repository_owners.get(&copy.repository) == Some(id)
                            && self.state.test_leases.get(id).is_some_and(|lease| {
                                lease.permits_registry_copy(
                                    &copy.repository,
                                    copy.node_id,
                                    copy.observed_at_unix_ms,
                                )
                            })
                    }
                    None => {
                        !reserved && !catalogue.repository_owners.contains_key(&copy.repository)
                    }
                };
                if !permitted {
                    return Some(CouncilResponse::Refused {
                        reason: "repository copy requires its active owner and writer receipt"
                            .into(),
                    });
                }
                if !self.state.manifest_catalog.add_manifest_holder(
                    &copy.repository,
                    &copy.manifest_digest,
                    copy.node_id,
                ) {
                    return Some(CouncilResponse::Refused {
                        reason: "repository manifest no longer exists".into(),
                    });
                }
            }
            RaftRequest::GcReport(report) => {
                if self
                    .state
                    .security_state
                    .crl
                    .retired_nodes
                    .keys()
                    .any(|name| crate::cluster::identity::raft_id_from_name(name) == report.node_id)
                {
                    return Some(CouncilResponse::Refused {
                        reason: "registry writer identity is retired".into(),
                    });
                }
                // The state machine is the deletion arbiter (M2): apply
                // runs serialised through the Raft log, so two nodes
                // racing to delete the last two copies of a layer get
                // their reports arbitrated in order — the second one is
                // refused the digest that would lose its final holder.
                let mut next = self.state.manifest_catalog.clone();
                let approved = next.apply_gc_report(report);
                if !approved.is_empty() {
                    let Some(generation) = self
                        .state
                        .registry_gc_generations
                        .get(&report.node_id)
                        .copied()
                        .unwrap_or(0)
                        .checked_add(1)
                    else {
                        return Some(CouncilResponse::Refused {
                            reason: "registry GC generation exhausted".into(),
                        });
                    };
                    self.state
                        .registry_gc_generations
                        .insert(report.node_id, generation);
                    self.state.manifest_catalog = next;
                }
                return Some(CouncilResponse::GcApproved { approved });
            }
            RaftRequest::DeleteTag(delete) => {
                self.state.manifest_catalog.apply_delete_tag(delete);
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
                let mut next = *sync_state.clone();
                let previous = self
                    .state
                    .gitops_sync_state
                    .as_ref()
                    .cloned()
                    .unwrap_or_default();
                // A stale run cannot erase triggers accepted while it was busy.
                next.requested_generation = previous.requested_generation;
                next.webhook_receipts = previous.webhook_receipts;
                next.completed_generation = next
                    .completed_generation
                    .min(next.requested_generation)
                    .max(previous.completed_generation);
                self.state.gitops_sync_state = Some(next);
            }
            RaftRequest::GitOpsSyncRequested { delivery } => {
                let sync = self
                    .state
                    .gitops_sync_state
                    .get_or_insert_with(Default::default);
                if sync.webhook_receipts.contains(delivery) {
                    return Some(CouncilResponse::Refused {
                        reason: "duplicate delivery ID (replay)".into(),
                    });
                }
                let Some(generation) = sync.requested_generation.checked_add(1) else {
                    return Some(CouncilResponse::Refused {
                        reason: "GitOps trigger generation exhausted".into(),
                    });
                };
                sync.requested_generation = generation;
                sync.webhook_receipts.push_back(*delivery);
                while sync.webhook_receipts.len() > 1000 {
                    sync.webhook_receipts.pop_front();
                }
                return Some(CouncilResponse::GitOpsSyncRequested { generation });
            }
            RaftRequest::AttachSignature(attach) => {
                // An unknown digest is refused, not silently dropped
                // (JOB7): the old no-op let a build report success while
                // its "signature" attached to nothing.
                if !self.state.manifest_catalog.apply_attach_signature(attach) {
                    return Some(CouncilResponse::Refused {
                        reason: format!(
                            "no manifest with digest {} in the catalogue",
                            attach.manifest_digest.as_str()
                        ),
                    });
                }
            }
            RaftRequest::SecurityStateInit(ss) => {
                // Init seeds a new cluster once. Replacing a live security
                // state wholesale would drop every token minted since, as a
                // recovered council re-seeding its bootstrap file did (#477).
                if self.state.security_state.is_initialised() {
                    return Some(CouncilResponse::Refused {
                        reason: "the cluster's security state is already initialised".to_string(),
                    });
                }
                self.state.security_state = *ss.clone();
            }
            RaftRequest::CreateJoinToken(jt) => {
                if self
                    .state
                    .security_state
                    .crl
                    .retired_nodes
                    .contains_key(&jt.node_id)
                {
                    return Some(CouncilResponse::Refused {
                        reason: "node identity is retired".into(),
                    });
                }
                // Prune consumed tokens and cap the list so it can't grow
                // without bound over a long-lived cluster (O5). Pruning keys on
                // `consumed` (replicated state) and a fixed cap, not wall-clock
                // expiry — a wall-clock check inside Raft apply would evaluate
                // differently on each node and diverge the state machine.
                // Expired-but-unconsumed tokens within the cap are harmless:
                // `check_join_token` rejects them.
                let tokens = &mut self.state.security_state.join_tokens;
                tokens.retain(|t| !t.consumed);
                tokens.push(jt.clone());
                const MAX_JOIN_TOKENS: usize = 1024;
                if tokens.len() > MAX_JOIN_TOKENS {
                    let overflow = tokens.len() - MAX_JOIN_TOKENS;
                    tokens.drain(0..overflow);
                }
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
            RaftRequest::ConsumeJoinTokenForIssue { token_hash } => {
                // Atomic consume + serial allocation (PKI5). Because the whole
                // log applies serially on every node, exactly one entry for a
                // given token finds it unconsumed; every racer and retry after
                // that is refused, so a token can only ever mint one serial.
                let token = self
                    .state
                    .security_state
                    .join_tokens
                    .iter_mut()
                    .find(|jt| jt.token_hash == *token_hash);
                let Some(token) = token else {
                    return Some(CouncilResponse::Refused {
                        reason: "join token not found".to_string(),
                    });
                };
                if self
                    .state
                    .security_state
                    .crl
                    .retired_nodes
                    .contains_key(&token.node_id)
                {
                    return Some(CouncilResponse::Refused {
                        reason: "node identity is retired".into(),
                    });
                }
                if token.consumed {
                    return Some(CouncilResponse::Refused {
                        reason: "join token already consumed".to_string(),
                    });
                }
                token.consumed = true;
                let node_id = token.node_id.clone();
                let serial = self.state.security_state.next_serial;
                self.state.security_state.next_serial += 1;
                crate::sesame::ca_rotation::record_node_leaf(
                    &mut self.state.security_state,
                    &node_id,
                    serial,
                );
                return Some(CouncilResponse::JoinTokenConsumed { serial });
            }
            RaftRequest::CreateApiToken(token) => {
                if token.name.starts_with("rbtest-")
                    || self
                        .state
                        .security_state
                        .api_tokens
                        .iter()
                        .any(|existing| existing.name == token.name)
                {
                    return Some(CouncilResponse::Refused {
                        reason: "token name already exists or requires a test lease".into(),
                    });
                }
                self.state.security_state.api_tokens.push(token.clone());
            }
            RaftRequest::RevokeApiToken { name } => {
                // Never let a revoke empty the store of Admin tokens. An empty
                // token store reopens the middleware's bootstrap allow-all
                // (`sesame::auth`), and the loopback-only bind guard is a
                // startup check — a node already bound to a routable address
                // would drop to fully unauthenticated at runtime. Applying
                // sequentially, this check is race-free: two concurrent revokes
                // of two distinct Admins become two entries, and the second one
                // (now the last Admin) is refused.
                use crate::sesame::types::ApiRole;
                let tokens = &self.state.security_state.api_tokens;
                let target_is_admin = tokens
                    .iter()
                    .any(|t| t.name == *name && t.role == ApiRole::Admin);
                let admin_count = tokens.iter().filter(|t| t.role == ApiRole::Admin).count();
                if target_is_admin && admin_count <= 1 {
                    return Some(CouncilResponse::Refused {
                        reason: format!(
                            "refusing to revoke {name:?}: it is the last Admin token; \
                             create a replacement Admin token before revoking this one"
                        ),
                    });
                }
                self.state
                    .security_state
                    .api_tokens
                    .retain(|t| t.name != *name);
            }
            RaftRequest::SweepExpiredApiTokens { now_unix_ms } => {
                // The entry carries the leader's clock, so replicas never
                // consult their own: all of them remove the same tokens.
                // `tokens_to_sweep` keeps the last Admin and never empties
                // the store, for the reason the revoke above refuses to.
                let removed = crate::sesame::token::tokens_to_sweep(
                    &self.state.security_state.api_tokens,
                    *now_unix_ms,
                );
                self.state
                    .security_state
                    .api_tokens
                    .retain(|token| !removed.contains(&token.name));
                return Some(CouncilResponse::ApiTokensSwept { removed });
            }
            RaftRequest::AllocateSerial => {
                // Return the pre-increment value as this entry's serial.
                let serial = self.state.security_state.next_serial;
                self.state.security_state.next_serial += 1;
                return Some(CouncilResponse::SerialAllocated { serial });
            }
            RaftRequest::RotateSecretKey {
                scope,
                new_keypair,
                resealed,
            } => {
                // Idempotent retry of the *same* rotation (deduped on the
                // new generation number): keep the first-applied keypair,
                // change nothing.
                let generation_exists = self
                    .state
                    .security_state
                    .age_keypairs
                    .iter()
                    .any(|kp| kp.scope == *scope && kp.generation == new_keypair.generation);
                if generation_exists {
                    return None;
                }
                // One rotation at a time (PKI8): a read-only key means an
                // earlier rotation hasn't been finalised. Stacking a third
                // generation on top multiplies the re-encrypt bookkeeping
                // and the ways to brick a secret — refuse instead.
                let rotation_in_flight = self
                    .state
                    .security_state
                    .age_keypairs
                    .iter()
                    .any(|kp| kp.scope == *scope && kp.read_only);
                if rotation_in_flight {
                    return Some(CouncilResponse::Refused {
                        reason: "a secret rotation for this scope is already in progress: \
                                 re-encrypt stored secrets and finalise it before starting another"
                            .to_string(),
                    });
                }
                let first_key = !self
                    .state
                    .security_state
                    .age_keypairs
                    .iter()
                    .any(|kp| kp.scope == *scope);
                if !resealed.is_empty()
                    && let Err(reason) = self.check_reseal(scope, first_key, resealed)
                {
                    return Some(CouncilResponse::Refused { reason });
                }
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
                if first_key && let AgeKeyScope::Namespace(namespace) = scope {
                    self.adopt_namespace_key(namespace, resealed);
                }
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
                if !has_active {
                    return Some(CouncilResponse::Refused {
                        reason: "no active replacement key exists for this scope: \
                                 start a rotation before finalising one"
                            .to_string(),
                    });
                }
                // Verify before retiring (PKI8): every stored secret in the
                // scope must be sealed under the newest generation, or the
                // retirement would brick it. Secrets without a recorded
                // seal count as "unknown generation" and block finalise
                // until re-encrypted.
                let newest = self
                    .state
                    .security_state
                    .age_keypairs
                    .iter()
                    .filter(|kp| kp.scope == *scope)
                    .map(|kp| kp.generation)
                    .max()
                    .unwrap_or(0);
                let stale = self.stale_sealed_secrets(scope, newest);
                if !stale.is_empty() {
                    return Some(CouncilResponse::Refused {
                        reason: format!(
                            "cannot finalise secret rotation: secrets still sealed under \
                             an old generation (re-encrypt and re-apply them first): {}",
                            stale.join(", ")
                        ),
                    });
                }
                // The cluster-wide generation-0 key stays, read-only: `relish
                // init` sealed the root CA's private key to it
                // (`<cluster>-root-ca.age`), and no seal record knows that, so
                // retiring it would leave the root backup unopenable (F04 R0).
                self.state
                    .security_state
                    .age_keypairs
                    .retain(|kp| kp.scope != *scope || !kp.read_only || opens_root_ca_backup(kp));
            }
            RaftRequest::RevokeCertificate(entry) => {
                self.state.security_state.crl.entries.push(entry.clone());
                // O5: drop entries whose certificates have since expired —
                // an expired certificate fails validation with or without a
                // CRL entry, so keeping it only grows every snapshot and
                // every handshake's scan.
                //
                // The clock is the incoming entry's own `revoked_at`, not
                // `SystemTime::now()`. Raft apply must be deterministic:
                // every replica applies this entry, and a wall-clock read
                // would have them prune different sets and diverge. A
                // timestamp carried *in the log* is the same on all of them.
                let logical_now = entry.revoked_at;
                self.state
                    .security_state
                    .crl
                    .entries
                    .retain(|e| e.expires_at.is_none_or(|expiry| expiry > logical_now));
                self.state.security_state.crl.version += 1;
                // Deterministic like the prune above (M12): a `SystemTime::now()`
                // here makes every replica store a different `updated_at`, so
                // the replicated state machines diverge on that field. Use the
                // in-log `revoked_at` so all replicas agree.
                self.state.security_state.crl.updated_at = logical_now;
            }
            RaftRequest::Noop => {}
            RaftRequest::QuotaBlocked { blocked } => {
                // Only apps still in desired state: a delete committed after
                // the leader planned must not leave a ghost reason behind.
                self.state.quota_blocked = blocked
                    .iter()
                    .filter(|(app_id, _)| self.state.apps.contains_key(app_id))
                    .cloned()
                    .collect();
            }
            RaftRequest::UpgradeUpdate { state } => {
                // Reject starting a *different* upgrade while one is actively in
                // progress (M13). The start/rollback handlers check
                // `active_upgrade` then write, which is racy — two concurrent
                // starts both passed the is_some() guard and the second
                // clobbered the first plan mid-flight. Serialised Raft apply is
                // the right place to enforce it.
                //
                // A resume legitimately renames the run to a fresh id and
                // replaces a *Paused* upgrade, so only a different-id write
                // against a non-paused active upgrade is the race to block; a
                // same-id phase update, or a resume of a paused run, still
                // applies.
                if let Some(active) = &self.state.active_upgrade
                    && active.upgrade_id != state.upgrade_id
                    && !matches!(
                        active.phase,
                        crate::upgrade::types::ClusterUpgradePhase::Paused { .. }
                    )
                {
                    return None;
                }
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
            RaftRequest::BatchRegister {
                expected_log_id,
                batch,
            } => {
                if *expected_log_id != position.previous_log_id {
                    return Some(CouncilResponse::Refused {
                        reason: "admission revision changed".into(),
                    });
                }
                if batch.jobs.iter().any(|job| {
                    job.node.as_ref().is_some_and(|node| {
                        self.state
                            .security_state
                            .crl
                            .retired_nodes
                            .contains_key(&node.0)
                    })
                }) {
                    return Some(CouncilResponse::Refused {
                        reason: "batch target node is retired".into(),
                    });
                }
                if batch.jobs.iter().any(|job| {
                    self.state.prerequisite_claims.values().any(|claim| {
                        // Display labels fence only held job operations. Physical
                        // execution names fence every target, including held apps.
                        claim.config.job.get(&job.name).is_some_and(|held_job| {
                            held_job.namespace.as_deref().unwrap_or("default") == job.namespace
                                && claim.blocks(&job.name, &job.namespace)
                        }) || claim.blocks(&job.execution_name, &job.namespace)
                    })
                }) {
                    return Some(CouncilResponse::Refused {
                        reason: "batch job identity is held by prerequisite ownership".into(),
                    });
                }
                if batch.jobs.iter().any(|job| {
                    self.state
                        .apps
                        .contains_key(&crate::meat::types::AppId::new(
                            &job.execution_name,
                            &job.namespace,
                        ))
                }) {
                    return Some(CouncilResponse::Refused {
                        reason: "execution identity already belongs to an app".into(),
                    });
                }
                return Some(match self.state.batch_state.register(batch.clone()) {
                    Ok(batch_id) => CouncilResponse::BatchRegistered { batch_id },
                    Err(reason) => CouncilResponse::Refused { reason },
                });
            }
            RaftRequest::BatchJobUpdate {
                batch_id,
                job_name,
                namespace,
                status,
                exit_code,
            } => {
                // Transition validation lives here, at the single point
                // every replica passes through: forged states, unknown
                // jobs and conflicting terminal reports are refused;
                // duplicate terminal reports apply as no-ops (JOB3).
                if let Err(e) = self
                    .state
                    .batch_state
                    .report(*batch_id, job_name, namespace, *status, *exit_code)
                {
                    return Some(CouncilResponse::Refused {
                        reason: e.to_string(),
                    });
                }
            }
            RaftRequest::BuildRegister { build } => {
                let build_id = self.state.build_state.register(build.clone());
                return Some(CouncilResponse::BuildRegistered { build_id });
            }
            RaftRequest::BuildUpdate { build_id, state } => {
                if let Err(e) = self.state.build_state.update(*build_id, state.clone()) {
                    return Some(CouncilResponse::Refused {
                        reason: e.to_string(),
                    });
                }
            }
            RaftRequest::NamespaceSpec { name, spec } => {
                #[cfg(test)]
                PREREQUISITE_NAMESPACE_VISITS.with(|visits| visits.set(visits.get() + 1));
                if crate::testkit::lease::valid_test_namespace(name) {
                    return Some(CouncilResponse::Refused {
                        reason: "test lease namespace requires a leased namespace write"
                            .to_string(),
                    });
                }
                self.state.namespaces.insert(name.clone(), *spec.clone());
            }
            RaftRequest::NamespaceDelete { name } => {
                self.state.namespaces.remove(name);
            }
            RaftRequest::PermissionSpec { name, spec } => {
                self.state.permissions.insert(name.clone(), *spec.clone());
            }
            RaftRequest::PermissionDelete { name } => {
                self.state.permissions.remove(name);
            }
            RaftRequest::RegisterEndpointConsumer { node_id } => {
                if let Err(reason) = crate::cluster::retirement::validate_node_id(node_id) {
                    return Some(CouncilResponse::Refused {
                        reason: reason.into(),
                    });
                }
                if self
                    .state
                    .security_state
                    .crl
                    .retired_nodes
                    .contains_key(node_id)
                {
                    return Some(CouncilResponse::Refused {
                        reason: "node identity is retired; fresh enrolment is required".into(),
                    });
                }
                if !self.state.endpoint_consumers.contains(node_id)
                    && self.state.endpoint_consumers.len()
                        >= crate::onion::catalog::MAX_ENDPOINT_CONSUMERS
                {
                    return Some(CouncilResponse::Refused {
                        reason: "endpoint consumer limit reached".into(),
                    });
                }
                self.state.endpoint_consumers.insert(node_id.clone());
            }
            RaftRequest::AcknowledgeEndpointWithdrawal {
                node_id,
                generation,
            } => {
                if let Err(reason) = crate::cluster::retirement::validate_node_id(node_id) {
                    return Some(CouncilResponse::Refused {
                        reason: reason.into(),
                    });
                }
                if self
                    .state
                    .security_state
                    .crl
                    .retired_nodes
                    .contains_key(node_id)
                {
                    return Some(CouncilResponse::Refused {
                        reason: "node identity is retired; fresh enrolment is required".into(),
                    });
                }
                if !self.state.endpoint_consumers.contains(node_id) {
                    return Some(CouncilResponse::Refused {
                        reason: "endpoint consumer is not registered".into(),
                    });
                }
                if *generation == 0 || *generation >= self.state.endpoint_withdrawals.generation {
                    return Some(CouncilResponse::Refused {
                        reason: "endpoint receipt must name an original withdrawn generation"
                            .into(),
                    });
                }
                // Generations never repeat. Retried historical receipts are no-ops;
                // they cannot discharge any current or later publication.
                let pending = &mut self.state.endpoint_withdrawals.pending;
                if let Some(withdrawal) = pending.get_mut(generation) {
                    withdrawal.consumers.remove(node_id);
                    if withdrawal.consumers.is_empty() {
                        pending.remove(generation);
                    }
                }
            }
            RaftRequest::DischargeEndpointConsumer { node_id } => {
                if let Err(reason) = crate::cluster::retirement::validate_node_id(node_id) {
                    return Some(CouncilResponse::Refused {
                        reason: reason.into(),
                    });
                }
                // The leader only proposes this once the consumer's own view
                // lease has run out (see `onion::lease`), so the node has
                // stopped routing. On its next poll it registers again and
                // republishes from scratch.
                if self.state.endpoint_consumers.remove(node_id) {
                    self.state.endpoint_withdrawals.retire_consumer(node_id);
                }
            }
            RaftRequest::RetireEndpointExecution { node_id, execution } => {
                if self
                    .state
                    .security_state
                    .crl
                    .retired_nodes
                    .contains_key(node_id)
                {
                    return Some(CouncilResponse::Refused {
                        reason: "node identity is retired; fresh enrolment is required".into(),
                    });
                }
                let retirements = match self
                    .state
                    .producer_retirements
                    .plan_retirement(node_id, execution)
                {
                    Ok(value) => value,
                    Err(error) => {
                        return Some(CouncilResponse::Refused {
                            reason: error.to_string(),
                        });
                    }
                };
                let catalog = retirements.withdraw(&self.state.endpoint_catalog);
                let withdrawals = match self.state.endpoint_withdrawals.plan_publication(
                    &self.state.endpoint_catalog,
                    &catalog,
                    &self.state.endpoint_consumers,
                ) {
                    Ok(value) => value,
                    Err(error) => {
                        return Some(CouncilResponse::Refused {
                            reason: error.to_string(),
                        });
                    }
                };
                let released = retirements.release_confirmed(node_id, execution, &withdrawals);
                self.state.producer_retirements = retirements;
                self.state.endpoint_catalog = catalog;
                self.state.endpoint_withdrawals = withdrawals;
                return Some(CouncilResponse::EndpointExecutionRetired { released });
            }
            RaftRequest::PublishEndpoints {
                expected_generation,
                catalog,
            } => {
                if *expected_generation != self.state.endpoint_withdrawals.generation {
                    return Some(CouncilResponse::Refused {
                        reason: format!(
                            "endpoint publication generation changed: expected {}, current {}; rebuild from committed state",
                            expected_generation, self.state.endpoint_withdrawals.generation,
                        ),
                    });
                }
                if catalog.services.values().any(|service| {
                    service.backends.iter().any(|backend| {
                        self.state
                            .security_state
                            .crl
                            .retired_nodes
                            .contains_key(&backend.node_id)
                    })
                }) {
                    return Some(CouncilResponse::Refused {
                        reason: "endpoint catalogue targets a retired node identity".into(),
                    });
                }
                if catalog.services.values().any(|service| {
                    service.backends.iter().any(|backend| {
                        self.state
                            .producer_retirements
                            .blocks(&backend.node_id, backend.execution.as_ref())
                    })
                }) {
                    return Some(CouncilResponse::Refused { reason: "endpoint catalogue targets a retired or uncorrelated producer execution".into() });
                }
                let withdrawals = match self.state.endpoint_withdrawals.plan_publication(
                    &self.state.endpoint_catalog,
                    catalog,
                    &self.state.endpoint_consumers,
                ) {
                    Ok(withdrawals) => withdrawals,
                    Err(error) => {
                        return Some(CouncilResponse::Refused {
                            reason: error.to_string(),
                        });
                    }
                };
                self.state.endpoint_withdrawals = withdrawals;
                self.state.endpoint_catalog = *catalog.clone();
            }
            RaftRequest::TestLeaseCreate(lease) => {
                if !lease.repositories.is_empty() || lease.workloads_retired {
                    return Some(CouncilResponse::Refused {
                        reason:
                            "a new lease cannot carry registry receipts or retirement confirmations"
                                .into(),
                    });
                }
                if lease.scope != crate::testkit::lease::LeaseScope::Applications {
                    return Some(CouncilResponse::Refused {
                        reason: "node job leases must remain on their owning node".to_string(),
                    });
                }
                if let Err(error) = lease.validate() {
                    return Some(CouncilResponse::Refused {
                        reason: error.to_string(),
                    });
                }
                if self.state.test_leases.contains_key(&lease.lease_id) {
                    return Some(CouncilResponse::Refused {
                        reason: "lease already exists".to_string(),
                    });
                }
                if self.state.test_leases.len() >= crate::testkit::lease::MAX_ACTIVE_TEST_LEASES {
                    return Some(CouncilResponse::Refused {
                        reason: "too many active test leases".to_string(),
                    });
                }
                if self
                    .state
                    .test_leases
                    .values()
                    .any(|existing| existing.namespace == lease.namespace)
                {
                    return Some(CouncilResponse::Refused {
                        reason: "lease namespace is already owned".to_string(),
                    });
                }
                self.state
                    .test_leases
                    .insert(lease.lease_id.clone(), lease.clone());
            }
            RaftRequest::TestLeaseRegistryWriter {
                lease_id,
                repository,
                node_id,
                owner_id,
                observed_at_unix_ms,
            } => {
                if self.registry_node_retired(*node_id) {
                    return Some(CouncilResponse::Refused {
                        reason: "registry writer identity is retired".into(),
                    });
                }
                let Some(lease) = self.state.test_leases.get_mut(lease_id) else {
                    return Some(CouncilResponse::Refused {
                        reason: "lease not found".into(),
                    });
                };
                if let Err(error) = lease.attach_registry_writer(
                    repository,
                    *node_id,
                    owner_id.as_deref(),
                    *observed_at_unix_ms,
                ) {
                    return Some(CouncilResponse::Refused {
                        reason: error.to_string(),
                    });
                }
            }
            RaftRequest::TestLeaseManifestCommit {
                lease_id,
                observed_at_unix_ms,
                commit,
            } => {
                if !self.registry_publication_is_current(commit) {
                    return Some(CouncilResponse::RegistryPublicationStale);
                }
                if self.registry_node_retired(commit.manifest.pushed_by)
                    || !self.state.test_leases.get(lease_id).is_some_and(|lease| {
                        lease.permits_registry_commit(commit, *observed_at_unix_ms)
                    })
                {
                    return Some(CouncilResponse::Refused {
                        reason: "repository lease or writer is not active".into(),
                    });
                }
                if let Err(error) = self
                    .state
                    .manifest_catalog
                    .claim_repository(&commit.manifest.repository, lease_id)
                {
                    return Some(CouncilResponse::Refused {
                        reason: error.to_string(),
                    });
                }
                self.state.manifest_catalog.apply_manifest_commit(commit);
            }
            RaftRequest::TestLeaseWorkloadsRetired { lease_id } => {
                let Some(lease) = self.state.test_leases.get(lease_id) else {
                    return Some(CouncilResponse::Refused {
                        reason: "lease not found".into(),
                    });
                };
                if !self.lease_workloads_absent(lease) {
                    return Some(CouncilResponse::Refused {
                        reason: "lease still owns workloads or placement obligations".into(),
                    });
                }
                if let Some(lease) = self.state.test_leases.get_mut(lease_id) {
                    lease.workloads_retired = true;
                }
            }
            RaftRequest::TestLeaseRegistryRetired {
                lease_id,
                repository,
                node_id,
            } => {
                let Some(lease) = self.state.test_leases.get_mut(lease_id) else {
                    return Some(CouncilResponse::Refused {
                        reason: "lease not found".into(),
                    });
                };
                if !lease.workloads_retired
                    || !matches!(
                        lease.state,
                        crate::testkit::lease::TestLeaseState::Cleaning { .. }
                    )
                {
                    return Some(CouncilResponse::Refused {
                        reason: "lease workloads have not retired".into(),
                    });
                }
                let Some(owners) = lease.repositories.get_mut(repository) else {
                    return Some(CouncilResponse::Refused {
                        reason: "repository does not belong to lease".into(),
                    });
                };
                owners.remove(node_id);
            }
            RaftRequest::TestLeaseAppSpec {
                lease_id,
                observed_at_unix_ms,
                app_id,
                spec,
            } => {
                if self
                    .state
                    .batch_state
                    .execution_owner(&app_id.namespace, &app_id.name)
                    .is_some()
                {
                    return Some(CouncilResponse::Refused {
                        reason: "identity belongs to a batch execution".into(),
                    });
                }
                if self
                    .state
                    .prerequisite_claims
                    .values()
                    .any(|claim| claim.blocks(&app_id.name, &app_id.namespace))
                {
                    return Some(CouncilResponse::Refused {
                        reason: "an outstanding prerequisite owns this app".into(),
                    });
                }

                let Some(lease) = self.state.test_leases.get(lease_id) else {
                    return Some(CouncilResponse::Refused {
                        reason: "lease not found".to_string(),
                    });
                };
                if !lease.is_active_at(*observed_at_unix_ms) {
                    return Some(CouncilResponse::Refused {
                        reason: "lease is expired or cleanup has started".to_string(),
                    });
                }
                if app_id.namespace != lease.namespace {
                    return Some(CouncilResponse::Refused {
                        reason: "app namespace does not match its lease".to_string(),
                    });
                }
                if let Err(error) = crate::testkit::lease::authorise_image_references(
                    spec.image_references(),
                    Some((lease, *observed_at_unix_ms)),
                ) {
                    return Some(CouncilResponse::Refused {
                        reason: error.to_string(),
                    });
                }
                let resource = crate::testkit::lease::LeasedResource::App {
                    app_id: app_id.clone(),
                };
                let already_owned = lease.resources.contains(&resource);
                if !already_owned
                    && lease.resource_count() >= crate::testkit::lease::MAX_LEASED_RESOURCES
                {
                    return Some(CouncilResponse::Refused {
                        reason: "lease resource limit reached".to_string(),
                    });
                }
                let owned_elsewhere = self.state.test_leases.iter().any(|(id, candidate)| {
                    id != lease_id && candidate.resources.contains(&resource)
                });
                if owned_elsewhere || (self.state.apps.contains_key(app_id) && !already_owned) {
                    return Some(CouncilResponse::Refused {
                        reason: "app already exists outside this lease".to_string(),
                    });
                }
                if let Some(lease) = self.state.test_leases.get_mut(lease_id) {
                    lease.resources.insert(resource);
                }
                self.apply_app_spec(app_id, spec);
            }
            RaftRequest::TestLeaseNamespaceSpec {
                lease_id,
                observed_at_unix_ms,
                name,
                spec,
            } => {
                let Some(lease) = self.state.test_leases.get(lease_id) else {
                    return Some(CouncilResponse::Refused {
                        reason: "lease not found".to_string(),
                    });
                };
                if !lease.is_active_at(*observed_at_unix_ms) {
                    return Some(CouncilResponse::Refused {
                        reason: "lease is expired or cleanup has started".to_string(),
                    });
                }
                if name != &lease.namespace {
                    return Some(CouncilResponse::Refused {
                        reason: "namespace does not match its lease".to_string(),
                    });
                }
                let resource =
                    crate::testkit::lease::LeasedResource::Namespace { name: name.clone() };
                let already_owned = lease.resources.contains(&resource);
                if !already_owned
                    && lease.resource_count() >= crate::testkit::lease::MAX_LEASED_RESOURCES
                {
                    return Some(CouncilResponse::Refused {
                        reason: "lease resource limit reached".to_string(),
                    });
                }
                if self.state.namespaces.contains_key(name) && !already_owned {
                    return Some(CouncilResponse::Refused {
                        reason: "namespace already exists outside this lease".to_string(),
                    });
                }
                if let Some(lease) = self.state.test_leases.get_mut(lease_id) {
                    lease.resources.insert(resource);
                }
                self.state.namespaces.insert(name.clone(), *spec.clone());
            }
            RaftRequest::TestLeaseApiToken {
                lease_id,
                owner_id,
                observed_at_unix_ms,
                token,
            } => {
                let Some(lease) = self.state.test_leases.get(lease_id) else {
                    return Some(CouncilResponse::Refused {
                        reason: "lease not found".into(),
                    });
                };
                let resource = match lease.token_resource(token, owner_id, *observed_at_unix_ms) {
                    Ok(resource) => resource,
                    Err(error) => {
                        return Some(CouncilResponse::Refused {
                            reason: error.to_string(),
                        });
                    }
                };
                let name_owned = self.state.test_leases.values().any(|lease| lease.resources.iter().any(|resource| {
                    matches!(resource, crate::testkit::lease::LeasedResource::ApiToken { name, .. } if name == &token.name)
                }));
                if name_owned
                    || self
                        .state
                        .security_state
                        .api_tokens
                        .iter()
                        .any(|existing| existing.name == token.name)
                {
                    return Some(CouncilResponse::Refused {
                        reason: "token name already exists or is lease-owned".into(),
                    });
                }
                if let Some(lease) = self.state.test_leases.get_mut(lease_id) {
                    lease.resources.insert(resource);
                }
                self.state.security_state.api_tokens.push(*token.clone());
            }
            RaftRequest::TestLeaseRevokeApiToken {
                lease_id,
                name,
                fingerprint,
            } => {
                let resource = crate::testkit::lease::LeasedResource::ApiToken {
                    name: name.clone(),
                    fingerprint: *fingerprint,
                };
                if !self.state.test_leases.get(lease_id).is_some_and(|lease| {
                    matches!(
                        lease.state,
                        crate::testkit::lease::TestLeaseState::Cleaning { .. }
                    ) && lease.resources.contains(&resource)
                }) {
                    return Some(CouncilResponse::Refused {
                        reason: "token is not owned by this cleaning lease".into(),
                    });
                }
                if self.state.security_state.api_tokens.iter().any(|token| {
                    &token.name == name
                        && (crate::testkit::lease::token_fingerprint(token) != *fingerprint
                            || token.role == crate::sesame::types::ApiRole::Admin)
                }) {
                    return Some(CouncilResponse::Refused {
                        reason: "owned token has been replaced; refusing to revoke it".into(),
                    });
                }
                self.state
                    .security_state
                    .api_tokens
                    .retain(|token| &token.name != name);
            }
            RaftRequest::TestLeaseRenew {
                lease_id,
                owner_id,
                renewed_at_unix_ms,
                expires_at_unix_ms,
            } => {
                let Some(lease) = self.state.test_leases.get_mut(lease_id) else {
                    return Some(CouncilResponse::Refused {
                        reason: "lease not found".to_string(),
                    });
                };
                if let Err(error) = lease.authorise_owner(owner_id, *renewed_at_unix_ms) {
                    return Some(CouncilResponse::Refused {
                        reason: error.to_string(),
                    });
                }
                if expires_at_unix_ms <= renewed_at_unix_ms {
                    return Some(CouncilResponse::Refused {
                        reason: "lease expiry must be in the future".to_string(),
                    });
                }
                lease.expires_at_unix_ms = *expires_at_unix_ms;
            }
            RaftRequest::TestLeaseBeginCleanup { lease_id } => {
                let Some(lease) = self.state.test_leases.get_mut(lease_id) else {
                    return Some(CouncilResponse::Refused {
                        reason: "lease not found".to_string(),
                    });
                };
                let attempts = match lease.state {
                    crate::testkit::lease::TestLeaseState::Active => 1,
                    crate::testkit::lease::TestLeaseState::Cleaning { attempts, .. } => {
                        attempts.saturating_add(1)
                    }
                };
                lease.state = crate::testkit::lease::TestLeaseState::Cleaning {
                    attempts,
                    last_error: None,
                };
            }
            RaftRequest::DecommissionNode {
                node_id,
                retired_by,
                reason,
                retired_at_unix_ms,
                membership_log_id,
            } => {
                use crate::cluster::retirement::{
                    DecommissionRequest, MAX_RETIRED_NODES, NodeRetirement,
                };
                let request = DecommissionRequest {
                    node_id: node_id.clone(),
                    workloads_stopped: true,
                    reason: reason.clone(),
                };
                if request.validate().is_err()
                    || retired_by.is_empty()
                    || retired_by.len() > 256
                    || *retired_at_unix_ms == 0
                {
                    return Some(CouncilResponse::Refused {
                        reason: "invalid node retirement record".into(),
                    });
                }
                if let Some(retirement) = self.state.security_state.crl.retired_nodes.get(node_id) {
                    return Some(CouncilResponse::NodeDecommissioned {
                        retirement: Box::new(retirement.clone()),
                    });
                }
                if self.state.security_state.crl.retired_nodes.len() >= MAX_RETIRED_NODES {
                    return Some(CouncilResponse::Refused {
                        reason: "retired identity limit reached".into(),
                    });
                }
                let membership = self.state.last_membership.membership();
                if self.state.last_membership.log_id() != membership_log_id
                    || membership.get_joint_config().len() > 1
                {
                    return Some(CouncilResponse::Refused {
                        reason: "membership changed; retry decommissioning".into(),
                    });
                }
                if self
                    .state
                    .node_fault_reservations
                    .active
                    .as_ref()
                    .is_some_and(|active| {
                        active.request.target_node.as_deref() != Some(node_id.as_str())
                    })
                {
                    return Some(CouncilResponse::Refused {
                        reason: "decommissioning waits for another node's fault reversal".into(),
                    });
                }
                for voters in membership.get_joint_config() {
                    let remaining = voters
                        .iter()
                        .filter(|id| {
                            membership.get_node(id).is_some_and(|node| {
                                node.name != *node_id
                                    && !self
                                        .state
                                        .security_state
                                        .crl
                                        .retired_nodes
                                        .contains_key(&node.name)
                            })
                        })
                        .count();
                    if !voters.is_empty() && remaining < voters.len() / 2 + 1 {
                        return Some(CouncilResponse::Refused { reason: "decommissioning would remove the remaining quorum; add replacement voters first".into() });
                    }
                }
                // Validate discovery retirement before changing any lease or membership evidence.
                let mut catalog = self.state.endpoint_catalog.clone();
                for service in catalog.services.values_mut() {
                    service
                        .backends
                        .retain(|backend| backend.node_id != *node_id);
                }
                let mut consumers = self.state.endpoint_consumers.clone();
                consumers.remove(node_id);
                let mut withdrawals = self.state.endpoint_withdrawals.clone();
                withdrawals.retire_consumer(node_id);
                let withdrawals = match withdrawals.plan_publication(
                    &self.state.endpoint_catalog,
                    &catalog,
                    &consumers,
                ) {
                    Ok(withdrawals) => withdrawals,
                    Err(error) => {
                        return Some(CouncilResponse::Refused {
                            reason: error.to_string(),
                        });
                    }
                };
                let mut released_placements = std::collections::BTreeMap::new();
                let mut released_registry_writers = std::collections::BTreeMap::new();
                let registry_node_id = crate::cluster::identity::raft_id_from_name(node_id);
                for (lease_id, lease) in &mut self.state.test_leases {
                    let mut registry_count = 0u64;
                    for owners in lease.repositories.values_mut() {
                        registry_count += u64::from(owners.remove(&registry_node_id));
                    }
                    if registry_count > 0 {
                        released_registry_writers.insert(lease_id.clone(), registry_count);
                    }
                    let before = lease.placements.len();
                    lease.placements.retain(|owner| owner.node_id.0 != *node_id);
                    let released = before - lease.placements.len();
                    if released > 0 {
                        released_placements.insert(lease_id.clone(), released as u64);
                    }
                }
                for placements in self.state.scheduling.values_mut() {
                    placements.retain(|placement| placement.node_id.0 != *node_id);
                }
                self.state.endpoint_catalog = catalog;
                self.state.endpoint_withdrawals = withdrawals;
                let released_node_fault = self
                    .state
                    .node_fault_reservations
                    .active
                    .take()
                    .map(|active| active.sequence);
                let released_endpoint_consumer = self.state.endpoint_consumers.remove(node_id);
                let retirement = NodeRetirement {
                    node_id: node_id.clone(),
                    retired_by: retired_by.clone(),
                    reason: reason.clone(),
                    retired_at_unix_ms: *retired_at_unix_ms,
                    released_placements,
                    released_registry_writers,
                    released_node_fault,
                    released_endpoint_consumer,
                };
                self.state
                    .security_state
                    .crl
                    .retired_nodes
                    .insert(node_id.clone(), retirement.clone());
                self.state.security_state.crl.version += 1;
                self.state.security_state.crl.updated_at = std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_millis(*retired_at_unix_ms);
                return Some(CouncilResponse::NodeDecommissioned {
                    retirement: Box::new(retirement),
                });
            }
            RaftRequest::AllocateNodeSerial { node_id } => {
                if self
                    .state
                    .security_state
                    .crl
                    .retired_nodes
                    .contains_key(node_id)
                {
                    return Some(CouncilResponse::Refused {
                        reason: "node identity is retired".into(),
                    });
                }
                let serial = self.state.security_state.next_serial;
                self.state.security_state.next_serial += 1;
                crate::sesame::ca_rotation::record_node_leaf(
                    &mut self.state.security_state,
                    node_id,
                    serial,
                );
                return Some(CouncilResponse::SerialAllocated { serial });
            }
            RaftRequest::CaRotationBegin { role, ca } => {
                return match crate::sesame::ca_rotation::begin(
                    &mut self.state.security_state,
                    *role,
                    ca,
                ) {
                    Ok(_) => None,
                    Err(error) => Some(CouncilResponse::Refused {
                        reason: error.to_string(),
                    }),
                };
            }
            RaftRequest::CaRotationPrepare {
                role,
                generation,
                csr_der,
                private_key_wrapped,
            } => {
                return Some(
                    match crate::sesame::ca_rotation::prepare(
                        &mut self.state.security_state,
                        *role,
                        *generation,
                        csr_der.clone(),
                        private_key_wrapped.clone(),
                    ) {
                        Ok(serial) => CouncilResponse::SerialAllocated { serial: serial.0 },
                        Err(error) => CouncilResponse::Refused {
                            reason: error.to_string(),
                        },
                    },
                );
            }
            RaftRequest::AcknowledgeNodeTrust {
                node_id,
                generation,
            } => {
                return match crate::sesame::ca_rotation::acknowledge_trust(
                    &mut self.state.security_state,
                    node_id,
                    *generation,
                ) {
                    Ok(()) => None,
                    Err(error) => Some(CouncilResponse::Refused {
                        reason: error.to_string(),
                    }),
                };
            }
            RaftRequest::CaRotationFinalize { role, now_unix_ms } => {
                let now = std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_millis(*now_unix_ms);
                return match crate::sesame::ca_rotation::finalize(
                    &mut self.state.security_state,
                    *role,
                    now,
                ) {
                    Ok(()) => None,
                    Err(error) => Some(CouncilResponse::Refused {
                        reason: error.to_string(),
                    }),
                };
            }
            RaftRequest::TestLeasePlacementRetired {
                lease_id,
                placement,
            } => {
                let Some(lease) = self.state.test_leases.get_mut(lease_id) else {
                    return Some(CouncilResponse::Refused {
                        reason: "lease not found".into(),
                    });
                };
                if !matches!(
                    lease.state,
                    crate::testkit::lease::TestLeaseState::Cleaning { .. }
                ) || self.state.apps.contains_key(&placement.app_id)
                    || !lease
                        .resources
                        .contains(&crate::testkit::lease::LeasedResource::App {
                            app_id: placement.app_id.clone(),
                        })
                {
                    return Some(CouncilResponse::Refused {
                        reason: "lease application is not retiring".into(),
                    });
                }
                // Retried acknowledgements are harmless; a different lease ID
                // cannot clear this generation's ownership.
                lease.placements.remove(placement);
            }
            RaftRequest::TestLeaseFinishCleanup { lease_id } => {
                let Some(lease) = self.state.test_leases.get(lease_id) else {
                    return Some(CouncilResponse::Refused {
                        reason: "lease not found".to_string(),
                    });
                };
                if !matches!(
                    lease.state,
                    crate::testkit::lease::TestLeaseState::Cleaning { .. }
                ) {
                    return Some(CouncilResponse::Refused {
                        reason: "lease cleanup has not started".to_string(),
                    });
                }
                if !lease.placements.is_empty() {
                    return Some(CouncilResponse::Refused {
                        reason: "lease still owns unconfirmed runtime placements".into(),
                    });
                }
                // Defence in depth against the cleanup-snapshot race: never
                // destroy the ownership record while a resource it owns still
                // exists. A driver that snapshotted the lease before an app
                // attached would otherwise finish here, orphaning that app with
                // no record left to reap it. Refusing keeps the record durable
                // so the next cleanup attempt re-reads and deletes it first.
                let resources: Vec<crate::testkit::lease::LeasedResource> =
                    lease.resources.iter().cloned().collect();
                let remaining: Vec<String> = resources
                    .iter()
                    .filter_map(|resource| match resource {
                        crate::testkit::lease::LeasedResource::Job { job_id } => {
                            Some(format!("node job {job_id}"))
                        }
                        crate::testkit::lease::LeasedResource::ApiToken { name, .. }
                            if self
                                .state
                                .security_state
                                .api_tokens
                                .iter()
                                .any(|token| &token.name == name) =>
                        {
                            Some(format!("token {name}"))
                        }
                        crate::testkit::lease::LeasedResource::App { app_id }
                            if self.state.apps.contains_key(app_id) =>
                        {
                            Some(app_id.to_string())
                        }
                        crate::testkit::lease::LeasedResource::Namespace { name }
                            if self.state.namespaces.contains_key(name) =>
                        {
                            Some(name.clone())
                        }
                        _ => None,
                    })
                    .collect();
                if !remaining.is_empty() {
                    return Some(CouncilResponse::Refused {
                        reason: format!(
                            "lease still owns live resources, refusing to finish cleanup: {}",
                            remaining.join(", ")
                        ),
                    });
                }
                if !lease.registry_retirement_confirmed() {
                    return Some(CouncilResponse::Refused {
                        reason: "lease still owns unconfirmed registry writers".into(),
                    });
                }
                // Check every generation before mutating any repository.
                for repository in lease.repositories.keys() {
                    if let Err(error) = self
                        .state
                        .manifest_catalog
                        .check_repository_owner(repository, lease_id)
                    {
                        return Some(CouncilResponse::Refused {
                            reason: error.to_string(),
                        });
                    }
                }
                for repository in lease.repositories.keys() {
                    self.state.manifest_catalog.retire_repository(repository);
                }
                self.state.test_leases.remove(lease_id);
            }
            RaftRequest::TestLeaseCleanupFailed { lease_id, reason } => {
                let Some(lease) = self.state.test_leases.get_mut(lease_id) else {
                    return Some(CouncilResponse::Refused {
                        reason: "lease not found".to_string(),
                    });
                };
                let attempts = match lease.state {
                    crate::testkit::lease::TestLeaseState::Cleaning { attempts, .. } => attempts,
                    crate::testkit::lease::TestLeaseState::Active => {
                        return Some(CouncilResponse::Refused {
                            reason: "lease cleanup has not started".to_string(),
                        });
                    }
                };
                lease.state = crate::testkit::lease::TestLeaseState::Cleaning {
                    attempts,
                    last_error: Some(reason.chars().take(512).collect()),
                };
            }
        }
        None
    }

    fn apply_app_spec(
        &mut self,
        app_id: &crate::meat::types::AppId,
        spec: &crate::config::app::AppSpec,
    ) {
        // A redeploy that changes the replica baseline invalidates any
        // autoscale override: the operator has re-declared the desired count.
        let baseline_changed = self
            .state
            .apps
            .get(app_id)
            .is_some_and(|old| old.replicas != spec.replicas);
        if baseline_changed {
            let key = app_id.to_string();
            self.state.autoscale_overrides.retain(|(k, _)| k != &key);
        }
        self.state.apps.insert(app_id.clone(), spec.clone());
        // Applying an app is how it starts again after `relish stop`.
        self.state.stopped_apps.remove(app_id);
        // Applying a spec is when encrypted values were re-sealed.
        self.record_secret_seals(app_id, spec);
    }

    /// The scope whose key seals `app_id`'s secrets: its namespace's key
    /// when one exists, the cluster-wide key otherwise — mirroring the
    /// agent's decrypt order (namespace first, then cluster-wide).
    fn effective_secret_scope(&self, app_id: &crate::meat::types::AppId) -> AgeKeyScope {
        let ns_scope = AgeKeyScope::Namespace(app_id.namespace.clone());
        let has_ns_key = self
            .state
            .security_state
            .age_keypairs
            .iter()
            .any(|kp| kp.scope == ns_scope);
        if has_ns_key {
            ns_scope
        } else {
            AgeKeyScope::ClusterWide
        }
    }

    /// Whether `resealed` may ride on this rotation (F05 I4): only a
    /// namespace's first key re-seals, only its own apps' values, and only
    /// values still exactly as the leader read them. An apply that landed
    /// in between makes the whole entry stale; the leader tries again.
    fn check_reseal(
        &self,
        scope: &AgeKeyScope,
        first_key: bool,
        resealed: &[crate::sesame::types::ResealedSecret],
    ) -> Result<(), String> {
        let AgeKeyScope::Namespace(namespace) = scope else {
            return Err("only a namespace's first secret key re-seals stored values".to_string());
        };
        if !first_key {
            return Err(format!(
                "namespace {namespace} already has a secret key; only its first key re-seals \
                 stored values"
            ));
        }
        for entry in resealed {
            let name = format!("{}/{}", entry.app_id, entry.env_key);
            if entry.app_id.namespace != *namespace {
                return Err(format!(
                    "cannot re-seal {name} under namespace {namespace}'s key: \
                     it belongs to another namespace"
                ));
            }
            if !crate::sesame::secret::is_encrypted(&entry.sealed) {
                return Err(format!("re-sealed value for {name} is not ENC[AGE:...]"));
            }
            let current = self
                .state
                .apps
                .get(&entry.app_id)
                .and_then(|spec| spec.env.get(&entry.env_key));
            let unchanged = current.is_some_and(|value| {
                value.is_encrypted() && value.as_str() == entry.previous.as_str()
            });
            if !unchanged {
                return Err(format!(
                    "stale re-seal: {name} changed after the leader read it"
                ));
            }
        }
        Ok(())
    }

    /// Switch `namespace` to its first key: write the re-sealed values and
    /// record every app's seals under the namespace scope, which is now
    /// its effective one (F05 I4).
    fn adopt_namespace_key(
        &mut self,
        namespace: &str,
        resealed: &[crate::sesame::types::ResealedSecret],
    ) {
        for entry in resealed {
            if let Some(spec) = self.state.apps.get_mut(&entry.app_id) {
                spec.env.insert(
                    entry.env_key.clone(),
                    crate::config::EnvValue::Encrypted(entry.sealed.clone()),
                );
            }
        }
        let in_namespace: Vec<(crate::meat::types::AppId, crate::config::app::AppSpec)> = self
            .state
            .apps
            .iter()
            .filter(|(app_id, _)| app_id.namespace == namespace)
            .map(|(app_id, spec)| (app_id.clone(), spec.clone()))
            .collect();
        for (app_id, spec) in &in_namespace {
            self.record_secret_seals(app_id, spec);
        }
    }

    /// Record the sealing generation for each of `spec`'s encrypted env
    /// values, replacing any previous records for the app (PKI8).
    fn record_secret_seals(
        &mut self,
        app_id: &crate::meat::types::AppId,
        spec: &crate::config::app::AppSpec,
    ) {
        let prefix = format!("{app_id}/");
        self.state
            .security_state
            .secret_seals
            .retain(|key, _| !key.starts_with(&prefix));

        let scope = self.effective_secret_scope(app_id);
        // No key material for the scope means nothing could have sealed
        // the values; record nothing (they'll block finalise as unknown).
        let Some(generation) = self
            .state
            .security_state
            .active_age_keypair(&scope)
            .map(|kp| kp.generation)
        else {
            return;
        };

        let encrypted_keys: Vec<String> = spec
            .env
            .iter()
            .filter(|(_, value)| value.is_encrypted())
            .map(|(key, _)| format!("{app_id}/{key}"))
            .collect();
        for key in encrypted_keys {
            self.state.security_state.secret_seals.insert(
                key,
                crate::sesame::types::SecretSeal {
                    scope: scope.clone(),
                    generation,
                },
            );
        }
    }

    /// Every stored secret in `scope` still sealed under a generation
    /// older than `newest`, including secrets with no recorded seal.
    /// Sorted, as
    /// `namespace/app/ENV_KEY` names, for deterministic error messages.
    fn stale_sealed_secrets(&self, scope: &AgeKeyScope, newest: u64) -> Vec<String> {
        let mut stale = Vec::new();
        for (app_id, spec) in &self.state.apps {
            for (env_key, value) in &spec.env {
                if !value.is_encrypted() {
                    continue;
                }
                let seal_key = format!("{app_id}/{env_key}");
                match self.state.security_state.secret_seals.get(&seal_key) {
                    Some(seal) => {
                        if &seal.scope == scope && seal.generation < newest {
                            stale.push(seal_key);
                        }
                    }
                    None => {
                        // Unknown generation: attribute it to the app's
                        // effective scope and treat it as needing
                        // re-encryption.
                        if &self.effective_secret_scope(app_id) == scope {
                            stale.push(seal_key);
                        }
                    }
                }
            }
        }
        stale.sort();
        stale
    }
}

// ---------------------------------------------------------------------------
// CouncilStateMachine
// ---------------------------------------------------------------------------

/// Raft state machine that applies entries to `DesiredState`.
///
/// Shared via `Arc<RwLock<_>>` so the snapshot builder can take a
/// read lock while the Raft core continues applying.
#[derive(Debug, Clone)]
pub struct CouncilStateMachine {
    inner: Arc<RwLock<StateMachineInner>>,
    gitops_triggers: Arc<tokio::sync::watch::Sender<(u64, u64)>>,
}

impl Default for CouncilStateMachine {
    fn default() -> Self {
        Self {
            inner: Arc::new(RwLock::new(StateMachineInner::default())),
            gitops_triggers: Self::trigger_sender(&DesiredState::default()),
        }
    }
}

impl CouncilStateMachine {
    fn trigger_sender(state: &DesiredState) -> Arc<tokio::sync::watch::Sender<(u64, u64)>> {
        let generations = state.gitops_sync_state.as_ref().map_or((0, 0), |sync| {
            (sync.requested_generation, sync.completed_generation)
        });
        Arc::new(tokio::sync::watch::channel(generations).0)
    }

    /// Observe committed requested and completed GitOps trigger generations.
    pub fn gitops_trigger_updates(&self) -> tokio::sync::watch::Receiver<(u64, u64)> {
        self.gitops_triggers.subscribe()
    }

    fn publish_gitops_triggers(&self, state: &DesiredState) {
        let generations = state.gitops_sync_state.as_ref().map_or((0, 0), |sync| {
            (sync.requested_generation, sync.completed_generation)
        });
        self.gitops_triggers.send_if_modified(|current| {
            if *current == generations {
                false
            } else {
                *current = generations;
                true
            }
        });
    }

    /// Create a new empty in-memory state machine (tests).
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a state machine backed by `db`, loading any persisted snapshot.
    ///
    /// On restart the loaded snapshot restores the applied state up to its
    /// boundary; openraft then replays the durable log's post-snapshot tail.
    /// A snapshot that exists but fails its checksum, carries an unknown
    /// format version, or won't decode is a hard error, never an empty state.
    // `SnapshotStoreError` is large but dictated by the redb/openraft errors
    // it wraps; boxing it here buys nothing.
    /// Whether a committed snapshot blob is present in `db`.
    ///
    /// [`with_store`](Self::with_store) loads an EMPTY `DesiredState` when no
    /// snapshot blob exists — the normal state of a young/low-churn cluster
    /// that has not yet crossed `snapshot_threshold`. Disaster recovery must
    /// distinguish "no snapshot yet" from a genuine one so it refuses to
    /// re-bootstrap an empty cluster instead of silently discarding all
    /// desired and security state.
    #[allow(clippy::result_large_err)]
    pub fn snapshot_present(db: &Database) -> Result<bool, SnapshotStoreError> {
        let rtx = db.begin_read()?;
        let table = match rtx.open_table(SNAPSHOT) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        Ok(table.get(SNAP_DATA_KEY)?.is_some())
    }

    #[allow(clippy::result_large_err)]
    pub fn with_store(db: Arc<Database>) -> Result<Self, SnapshotStoreError> {
        // Only a genuinely empty store needs a write before validation.
        let table_exists = {
            let rtx = db.begin_read()?;
            match rtx.open_table(SNAPSHOT) {
                Ok(_) => true,
                Err(redb::TableError::TableDoesNotExist(_)) => false,
                Err(error) => return Err(error.into()),
            }
        };
        if !table_exists {
            let wtx = db.begin_write()?;
            {
                wtx.open_table(SNAPSHOT)?;
            }
            wtx.commit()?;
        }

        let mut inner = StateMachineInner::default();
        {
            let rtx = db.begin_read()?;
            let t = rtx.open_table(SNAPSHOT)?;
            if let Some(data) = t.get(SNAP_DATA_KEY)? {
                let bytes = data.value().to_vec();
                match read_snapshot_version(&t)? {
                    None => {
                        return Err(SnapshotStoreError::UnsupportedVersion {
                            found: 0,
                            supported: SNAPSHOT_FORMAT_VERSION,
                        });
                    }
                    Some(SNAPSHOT_FORMAT_VERSION) => verify_snapshot_checksum(&t, &bytes)?,
                    Some(found) => {
                        return Err(SnapshotStoreError::UnsupportedVersion {
                            found,
                            supported: SNAPSHOT_FORMAT_VERSION,
                        });
                    }
                }
                // A snapshot blob that EXISTS but won't decode is corruption,
                // not absence. Fail closed (CP3): after log compaction the
                // entries this snapshot covers are gone, so silently booting an
                // empty state here would destroy the cluster's desired and
                // security state (app specs, CA material, tokens). Refuse to
                // start so an operator can restore from backup instead.
                inner.state = serde_json::from_slice::<DesiredState>(&bytes)?;
                inner.snapshot_data = Some(bytes);
                inner.snapshot_last_log_id = inner.state.last_applied_log;
                inner.snapshot_membership = inner.state.last_membership.clone();
            }
            if let Some(idx) = t.get(SNAP_INDEX_KEY)?
                && let Ok(le) = <[u8; 8]>::try_from(idx.value())
            {
                inner.snapshot_index = u64::from_le_bytes(le);
            }
        }
        inner.db = Some(db);
        Ok(Self {
            gitops_triggers: Self::trigger_sender(&inner.state),
            inner: Arc::new(RwLock::new(inner)),
        })
    }

    /// Build an in-memory state machine seeded with a restored `DesiredState`
    /// for disaster recovery (12b.2 D21/CP12).
    ///
    /// The restored state comes from an external backup (or a survivor's own
    /// durable snapshot). Its `last_applied_log` and `last_membership` are
    /// cleared so the fresh single-voter Raft that wraps this state machine
    /// starts a clean log rather than inheriting the dead cluster's term line.
    /// The recovery epoch is bumped so post-recovery state is distinguishable
    /// from anything issued before the loss.
    pub fn from_recovered_state(mut state: DesiredState) -> Self {
        // A recovered node re-bootstraps its own Raft: the old log id and
        // membership belong to the cluster that died, so we drop them and let
        // `initialize` establish a fresh term line and voter set.
        state.enter_recovery_epoch();

        let inner = StateMachineInner {
            state,
            ..StateMachineInner::default()
        };
        Self {
            gitops_triggers: Self::trigger_sender(&inner.state),
            inner: Arc::new(RwLock::new(inner)),
        }
    }

    /// Offline disaster recovery (12b.2 D21/CP12): stamp a restored
    /// `DesiredState` into the durable snapshot store at `snapshot_db`,
    /// bumping the recovery epoch and clearing the dead cluster's log id and
    /// membership. The next node start loads this snapshot via
    /// [`CouncilStateMachine::with_store`] and re-bootstraps a fresh Raft.
    ///
    /// The write uses the same enveloped format (version + checksum) as a
    /// normal snapshot, so nothing downstream can tell a recovered snapshot
    /// from an ordinary one except by the bumped `recovery_epoch`.
    // `redb::Error` is large but dictated by the crate; boxing it buys nothing.
    #[allow(clippy::result_large_err)]
    pub fn persist_recovered_snapshot(
        snapshot_db: &Database,
        mut state: DesiredState,
    ) -> Result<(), redb::Error> {
        state.enter_recovery_epoch();
        // Serialisation of a plain struct with only owned data is infallible in
        // practice; map any error into a redb error rather than panicking.
        let data = serde_json::to_vec(&state)
            .map_err(|e| redb::Error::Io(std::io::Error::other(e.to_string())))?;
        persist_snapshot(snapshot_db, &data, 1)
    }

    /// An offline recovery snapshot has no old log boundary or membership.
    /// Ordinary fresh joiners have no snapshot and must still await their seed.
    pub async fn recovered_bootstrap_pending(&self) -> bool {
        let guard = self.inner.read().await;
        guard.snapshot_data.is_some()
            && guard.state.recovery_epoch > 0
            && guard.state.last_applied_log.is_none()
            && guard
                .state
                .last_membership
                .membership()
                .nodes()
                .next()
                .is_none()
    }

    /// Whether a snapshot was loaded or taken, as opposed to the empty state
    /// of a store that never snapshotted.
    pub async fn holds_snapshot(&self) -> bool {
        self.inner.read().await.snapshot_data.is_some()
    }

    /// Read the current desired state.
    pub async fn desired_state(&self) -> DesiredState {
        self.inner.read().await.state.clone()
    }

    /// Read part of the desired state under the lock, without cloning all of it.
    pub async fn read_desired<T>(&self, read: impl FnOnce(&DesiredState) -> T) -> T {
        read(&self.inner.read().await.state)
    }

    /// Log id the loaded snapshot covers up to, `None` when no snapshot is
    /// loaded. Only meaningful straight after `with_store`, before Raft
    /// applies new entries; `council::validate_purge_boundary` reads it at
    /// startup to check the snapshot still covers the purged log prefix.
    pub async fn snapshot_last_applied(&self) -> Option<LogId<u64>> {
        let guard = self.inner.read().await;
        if guard.snapshot_data.is_some() {
            guard.snapshot_last_log_id
        } else {
            None
        }
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
            let position = ApplyEntryPosition {
                previous_log_id: guard.state.last_applied_log,
                current_log_id: Some(log_id),
            };
            guard.state.last_applied_log = Some(log_id);

            match entry.payload {
                EntryPayload::Blank => {
                    responses.push(CouncilResponse::Applied {
                        log_index: log_id.index,
                    });
                }
                EntryPayload::Normal(request) => {
                    let response = guard.apply_request_at(&request, position);
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
        self.publish_gitops_triggers(&guard.state);
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

        let (db, index) = {
            let mut guard = self.inner.write().await;
            guard.state = new_state;
            guard.state.last_applied_log = meta.last_log_id;
            guard.state.last_membership = meta.last_membership.clone();
            self.publish_gitops_triggers(&guard.state);
            guard.snapshot_index += 1;
            guard.snapshot_data = Some(data.clone());
            guard.snapshot_last_log_id = meta.last_log_id;
            guard.snapshot_membership = meta.last_membership.clone();
            (guard.db.clone(), guard.snapshot_index)
        };

        // Persist the installed snapshot so it survives a restart too, off the
        // async runtime (M7): the lock is released before the fsync.
        if let Some(db) = db {
            persist_snapshot_blocking(db, data, index).await?;
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
                    last_log_id: guard.snapshot_last_log_id,
                    last_membership: guard.snapshot_membership.clone(),
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
        let (data, index, db, last_log_id, last_membership) = {
            let mut guard = self.inner.write().await;
            let data = serde_json::to_vec(&guard.state)
                .map_err(|e| StorageError::from(StorageIOError::read_state_machine(&e)))?;
            guard.snapshot_index += 1;
            guard.snapshot_data = Some(data.clone());
            guard.snapshot_last_log_id = guard.state.last_applied_log;
            guard.snapshot_membership = guard.state.last_membership.clone();
            (
                data,
                guard.snapshot_index,
                guard.db.clone(),
                guard.state.last_applied_log,
                guard.state.last_membership.clone(),
            )
        };

        // Persist so applied state survives a restart, off the async runtime
        // (M7): the lock is released before the fsync.
        if let Some(db) = db {
            persist_snapshot_blocking(db, data.clone(), index).await?;
        }

        let meta = SnapshotMeta {
            last_log_id,
            last_membership,
            snapshot_id: format!("mem-{index}"),
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

    impl CouncilStateMachine {
        // Existing fixtures describe current plans, not a stale admission race.
        // Fill their CAS boundary from the preceding entry; explicit CAS tests
        // and the legacy-refusal control use the real apply method directly.
        async fn apply_fixture<I>(
            &mut self,
            entries: I,
        ) -> Result<Vec<CouncilResponse>, Box<StorageError<u64>>>
        where
            I: IntoIterator<Item = openraft::Entry<TypeConfig>> + Send,
            I::IntoIter: Send,
        {
            let mut previous = self.desired_state().await.last_applied_log;
            let entries: Vec<_> = entries
                .into_iter()
                .map(|mut entry| {
                    if let EntryPayload::Normal(request) = &mut entry.payload {
                        match request {
                            RaftRequest::SchedulingDecision(decision) => {
                                *request = RaftRequest::SchedulingDecisions {
                                    expected_log_id: previous,
                                    decisions: vec![decision.clone()],
                                }
                            }
                            RaftRequest::BatchRegister {
                                expected_log_id, ..
                            } => *expected_log_id = previous,
                            _ => {}
                        }
                    }
                    previous = Some(entry.log_id);
                    entry
                })
                .collect();
            self.apply(entries).await.map_err(Box::new)
        }
    }

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

    /// #429: only an offline recovery snapshot (no log boundary, no
    /// membership) may bootstrap past join seeds. A joiner that installed the
    /// recovered council's snapshot holds the same epoch but a log position,
    /// and must never bootstrap a council of its own.
    #[tokio::test]
    async fn only_an_offline_recovery_snapshot_is_a_pending_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::create(dir.path().join("snapshot.redb")).unwrap());
        CouncilStateMachine::persist_recovered_snapshot(&db, DesiredState::default()).unwrap();
        let mut recovered = CouncilStateMachine::with_store(db).unwrap();
        assert!(recovered.recovered_bootstrap_pending().await);
        assert!(
            !CouncilStateMachine::new()
                .recovered_bootstrap_pending()
                .await
        );

        recovered
            .apply_fixture(vec![normal_entry(1, 1, RaftRequest::Noop)])
            .await
            .unwrap();
        let snapshot = recovered
            .get_snapshot_builder()
            .await
            .build_snapshot()
            .await
            .unwrap();
        let mut joiner = CouncilStateMachine::new();
        joiner
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        assert_eq!(joiner.desired_state().await.recovery_epoch, 1);
        assert!(!joiner.recovered_bootstrap_pending().await);
    }

    #[tokio::test]
    async fn node_fault_capacity_survives_snapshot_and_leader_term_changes() {
        use crate::smoker::reservation::NodeFaultReservation;
        use crate::smoker::types::{FaultRequest, FaultType};
        let mut sm = CouncilStateMachine::new();
        sm.apply_fixture(vec![openraft::Entry {
            log_id: log_id(1, 1),
            payload: EntryPayload::Membership(Membership::new(
                vec![std::collections::BTreeSet::from([1, 2, 3])],
                None::<std::collections::BTreeSet<u64>>,
            )),
        }])
        .await
        .unwrap();
        let grant = NodeFaultReservation {
            sequence: 1,
            boot_id: "boot-a".into(),
            cleanup_after_unix_ms: 1,
            request: FaultRequest {
                fault_type: FaultType::NodeKill {
                    kill_containers: false,
                },
                target_node: Some("node-a".into()),
                target_service: String::new(),
                target_instance: None,
                namespace: None,
                duration: std::time::Duration::from_secs(1),
                injected_by: "operator".into(),
                reason: None,
                include_leader: true,
                override_safety: true,
                acknowledged: true,
            },
        };
        let reserve = |reservation: NodeFaultReservation, membership_log_id, unavailable_voters| {
            RaftRequest::ReserveNodeFault {
                reservation: Box::new(reservation),
                membership_log_id,
                unavailable_voters,
            }
        };
        let stale = sm
            .apply_fixture(vec![normal_entry(
                1,
                2,
                reserve(grant.clone(), None, Default::default()),
            )])
            .await
            .unwrap();
        assert!(matches!(stale[0], CouncilResponse::Refused { .. }));
        let risk = sm
            .apply_fixture(vec![normal_entry(
                1,
                3,
                reserve(grant.clone(), Some(log_id(1, 1)), [2].into()),
            )])
            .await
            .unwrap();
        assert!(matches!(risk[0], CouncilResponse::Refused { .. }));
        let admitted = sm
            .apply_fixture(vec![normal_entry(
                1,
                4,
                reserve(grant.clone(), Some(log_id(1, 1)), Default::default()),
            )])
            .await
            .unwrap();
        assert!(matches!(admitted[0], CouncilResponse::Applied { .. }));
        let mut builder = sm.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.unwrap();
        let mut restored = CouncilStateMachine::new();
        restored
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        let mut other = grant.clone();
        other.sequence = 2;
        other.request.target_node = Some("node-b".into());
        let refused = restored
            .apply_fixture(vec![normal_entry(
                2,
                5,
                reserve(other.clone(), Some(log_id(1, 1)), Default::default()),
            )])
            .await
            .unwrap();
        assert!(matches!(refused[0], CouncilResponse::Refused { .. }));
        assert_eq!(
            restored
                .desired_state()
                .await
                .node_fault_reservations
                .active,
            Some(grant)
        );
        restored
            .apply_fixture(vec![normal_entry(
                2,
                6,
                RaftRequest::ReleaseNodeFault { sequence: 1 },
            )])
            .await
            .unwrap();
        let admitted = restored
            .apply_fixture(vec![normal_entry(
                2,
                7,
                reserve(other, Some(log_id(1, 1)), Default::default()),
            )])
            .await
            .unwrap();
        assert!(matches!(admitted[0], CouncilResponse::Applied { .. }));
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

        let responses = sm.apply_fixture(vec![entry]).await.unwrap();
        assert_eq!(responses.len(), 1);

        let state = sm.desired_state().await;
        assert_eq!(state.apps.get(&app_id).unwrap().image, spec.image);
    }

    /// #307: redeploying an app with a changed ingress host replaces the
    /// route in the council's catalogue, and dropping the ingress removes it.
    #[tokio::test]
    async fn redeploying_with_a_changed_ingress_host_replaces_the_cluster_route() {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "prod");
        let mut hosts = Vec::new();
        for (index, host) in [Some("a.test"), Some("b.test"), None]
            .into_iter()
            .enumerate()
        {
            let spec = AppSpec {
                ingress: host.map(|host| toml::from_str(&format!("host = '{host}'")).unwrap()),
                ..default_spec()
            };
            let entry = normal_entry(
                1,
                index as u64 + 1,
                RaftRequest::AppSpec {
                    app_id: app_id.clone(),
                    spec: Box::new(spec),
                },
            );
            sm.apply_fixture(vec![entry]).await.unwrap();
            let routes = crate::cluster::orchestrate::cluster_ingress(&sm.desired_state().await);
            hosts.push(
                routes
                    .into_iter()
                    .map(|route| route.config.host)
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(
            hosts,
            vec![
                vec!["a.test".to_string()],
                vec!["b.test".to_string()],
                vec![]
            ]
        );
    }

    #[tokio::test]
    async fn stop_keeps_the_spec_until_the_next_apply_and_delete_forgets_both() {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "prod");
        let spec = AppSpec {
            image: Some("myapp:v2".to_string()),
            ..default_spec()
        };
        let upsert = |index| {
            normal_entry(
                1,
                index,
                RaftRequest::AppSpec {
                    app_id: app_id.clone(),
                    spec: Box::new(spec.clone()),
                },
            )
        };
        let stop = |index| {
            normal_entry(
                1,
                index,
                RaftRequest::AppStop {
                    app_id: app_id.clone(),
                },
            )
        };
        let refused = sm.apply_fixture(vec![stop(1)]).await.unwrap();
        assert!(matches!(refused[0], CouncilResponse::Refused { .. }));

        sm.apply_fixture(vec![upsert(2), stop(3)]).await.unwrap();
        let state = sm.desired_state().await;
        assert!(state.apps.contains_key(&app_id));
        assert!(state.stopped_apps.contains(&app_id));

        sm.apply_fixture(vec![upsert(4)]).await.unwrap();
        assert!(!sm.desired_state().await.stopped_apps.contains(&app_id));

        let delete = normal_entry(
            1,
            6,
            RaftRequest::AppDelete {
                app_id: app_id.clone(),
            },
        );
        sm.apply_fixture(vec![stop(5), delete]).await.unwrap();
        let state = sm.desired_state().await;
        assert!(!state.apps.contains_key(&app_id));
        assert!(!state.stopped_apps.contains(&app_id));
    }

    /// A managed volume stays on its node, so the cluster has to remember
    /// where a stopped app ran after its placements have gone.
    #[tokio::test]
    async fn a_stop_keeps_the_nodes_the_app_last_ran_on_until_delete() {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "prod");
        let decision = |index, nodes: &[&str]| {
            normal_entry(
                1,
                index,
                RaftRequest::SchedulingDecision(SchedulingDecision {
                    app_id: app_id.clone(),
                    placements: nodes
                        .iter()
                        .zip(0..)
                        .map(|(node, ordinal)| Placement {
                            node_id: NodeId::new(*node),
                            resources: Resources::new(100, 0, 0),
                            ordinal,
                        })
                        .collect(),
                }),
            )
        };
        let upsert = normal_entry(
            1,
            1,
            RaftRequest::AppSpec {
                app_id: app_id.clone(),
                spec: Box::new(default_spec()),
            },
        );
        let stop = normal_entry(
            1,
            3,
            RaftRequest::AppStop {
                app_id: app_id.clone(),
            },
        );
        // The stopped app's decision places nothing.
        sm.apply_fixture(vec![
            upsert,
            decision(2, &["node-2"]),
            stop,
            decision(4, &[]),
        ])
        .await
        .unwrap();
        let state = sm.desired_state().await;
        assert!(state.scheduling[&app_id].is_empty());
        assert_eq!(state.last_placed_nodes[&app_id], [NodeId::new("node-2")]);

        sm.apply_fixture(vec![decision(5, &["node-3"])])
            .await
            .unwrap();
        let state = sm.desired_state().await;
        assert_eq!(state.last_placed_nodes[&app_id], [NodeId::new("node-3")]);

        let delete = normal_entry(
            1,
            6,
            RaftRequest::AppDelete {
                app_id: app_id.clone(),
            },
        );
        sm.apply_fixture(vec![delete]).await.unwrap();
        assert!(
            !sm.desired_state()
                .await
                .last_placed_nodes
                .contains_key(&app_id)
        );
    }

    /// Two replicas named `web-1` on different nodes is the bug #398 fixed;
    /// the log refuses a decision that would bring it back.
    #[tokio::test]
    async fn a_decision_with_a_shared_ordinal_is_refused() {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "prod");
        let placement = |node: &str, ordinal| Placement {
            node_id: NodeId::new(node),
            resources: Resources::new(100, 0, 0),
            ordinal,
        };
        let decision = |index, placements| {
            normal_entry(
                1,
                index,
                RaftRequest::SchedulingDecision(SchedulingDecision {
                    app_id: app_id.clone(),
                    placements,
                }),
            )
        };
        let refused = sm
            .apply_fixture(vec![decision(
                1,
                vec![placement("node-1", 1), placement("node-2", 1)],
            )])
            .await
            .unwrap();
        assert!(matches!(refused[0], CouncilResponse::Refused { .. }));
        assert!(!sm.desired_state().await.scheduling.contains_key(&app_id));

        // Listed out of ordinal order, the nodes are still remembered in it.
        sm.apply_fixture(vec![decision(
            2,
            vec![placement("node-2", 1), placement("node-1", 0)],
        )])
        .await
        .unwrap();
        assert_eq!(
            sm.desired_state().await.last_placed_nodes[&app_id],
            [NodeId::new("node-1"), NodeId::new("node-2")]
        );
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
            sm.apply_fixture(vec![normal_entry(
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

    /// Persist a small known state to `path` via the real snapshot path,
    /// returning the app id it contains. Everything is dropped before
    /// returning so the caller can reopen (or tamper with) the store.
    async fn persist_known_snapshot(path: &std::path::Path) -> AppId {
        let app_id = AppId::new("web", "prod");
        let db = std::sync::Arc::new(Database::create(path).unwrap());
        let mut sm = CouncilStateMachine::with_store(db).unwrap();
        sm.apply_fixture(vec![normal_entry(
            1,
            1,
            RaftRequest::AppSpec {
                app_id: app_id.clone(),
                spec: Box::new(default_spec()),
            },
        )])
        .await
        .unwrap();
        let mut builder = sm.get_snapshot_builder().await;
        builder.build_snapshot().await.unwrap();
        app_id
    }

    /// Flip one byte in the middle of the stored value under `key`.
    fn flip_stored_byte(path: &std::path::Path, key: &str) {
        let db = Database::create(path).unwrap();
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

    #[tokio::test]
    async fn persisted_snapshot_carries_version_and_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("envelope.redb");
        persist_known_snapshot(&path).await;

        let db = Database::create(&path).unwrap();
        let rtx = db.begin_read().unwrap();
        let t = rtx.open_table(SNAPSHOT).unwrap();
        let version = t.get(SNAP_VERSION_KEY).unwrap().unwrap().value().to_vec();
        assert_eq!(
            version,
            SNAPSHOT_FORMAT_VERSION.to_le_bytes().to_vec(),
            "the persisted snapshot records the format version"
        );
        assert!(
            t.get(SNAP_CHECKSUM_KEY).unwrap().is_some(),
            "the persisted snapshot records a payload checksum"
        );
    }

    #[tokio::test]
    async fn with_store_fails_closed_on_a_flipped_payload_byte() {
        // Bit-rot in the payload: whether or not the damaged bytes still
        // parse as JSON, the checksum must catch it and refuse startup.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bitrot.redb");
        persist_known_snapshot(&path).await;
        flip_stored_byte(&path, SNAP_DATA_KEY);

        let db = std::sync::Arc::new(Database::create(&path).unwrap());
        let err = CouncilStateMachine::with_store(db).unwrap_err();
        assert!(
            matches!(err, SnapshotStoreError::ChecksumMismatch { .. }),
            "expected a checksum mismatch, got: {err}"
        );
    }

    #[tokio::test]
    async fn with_store_fails_closed_on_a_flipped_checksum_byte() {
        // Same failure mode when the rot lands in the checksum itself.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sumrot.redb");
        persist_known_snapshot(&path).await;
        flip_stored_byte(&path, SNAP_CHECKSUM_KEY);

        let db = std::sync::Arc::new(Database::create(&path).unwrap());
        let err = CouncilStateMachine::with_store(db).unwrap_err();
        assert!(
            matches!(err, SnapshotStoreError::ChecksumMismatch { .. }),
            "expected a checksum mismatch, got: {err}"
        );
    }

    #[tokio::test]
    async fn with_store_rejects_an_unknown_snapshot_version() {
        // A snapshot written by a future binary must refuse to load rather
        // than be misinterpreted by this one.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.redb");
        persist_known_snapshot(&path).await;
        {
            let db = Database::create(&path).unwrap();
            let wtx = db.begin_write().unwrap();
            {
                let mut t = wtx.open_table(SNAPSHOT).unwrap();
                t.insert(SNAP_VERSION_KEY, 99u32.to_le_bytes().as_slice())
                    .unwrap();
            }
            wtx.commit().unwrap();
        }

        let db = std::sync::Arc::new(Database::create(&path).unwrap());
        let err = CouncilStateMachine::with_store(db).unwrap_err();
        assert!(
            matches!(
                err,
                SnapshotStoreError::UnsupportedVersion {
                    found: 99,
                    supported: SNAPSHOT_FORMAT_VERSION
                }
            ),
            "expected an unsupported-version error, got: {err}"
        );
    }

    #[tokio::test]
    async fn unversioned_snapshot_is_refused_and_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unversioned.redb");
        let app_id = AppId::new("web", "prod");
        let mut unversioned_state = DesiredState::default();
        unversioned_state.apps.insert(
            app_id.clone(),
            AppSpec {
                image: Some("unversioned:v1".to_string()),
                ..default_spec()
            },
        );
        unversioned_state.last_applied_log = Some(log_id(1, 3));
        let payload = serde_json::to_vec(&unversioned_state).unwrap();
        {
            let db = Database::create(&path).unwrap();
            let wtx = db.begin_write().unwrap();
            {
                let mut t = wtx.open_table(SNAPSHOT).unwrap();
                t.insert(SNAP_DATA_KEY, payload.as_slice()).unwrap();
                t.insert(SNAP_INDEX_KEY, 3u64.to_le_bytes().as_slice())
                    .unwrap();
            }
            wtx.commit().unwrap();
        }

        let db = std::sync::Arc::new(Database::create(&path).unwrap());
        assert!(matches!(
            CouncilStateMachine::with_store(db.clone()),
            Err(SnapshotStoreError::UnsupportedVersion { found: 0, .. })
        ));
        let rtx = db.begin_read().unwrap();
        let table = rtx.open_table(SNAPSHOT).unwrap();
        assert_eq!(table.get(SNAP_DATA_KEY).unwrap().unwrap().value(), payload);
        assert!(table.get(SNAP_VERSION_KEY).unwrap().is_none());
        assert!(table.get(SNAP_CHECKSUM_KEY).unwrap().is_none());
    }

    #[tokio::test]
    async fn snapshot_last_applied_is_none_without_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(Database::create(dir.path().join("bare.redb")).unwrap());
        let sm = CouncilStateMachine::with_store(db).unwrap();
        assert_eq!(sm.snapshot_last_applied().await, None);
    }

    #[test]
    fn copy_confirmation_adds_only_its_node_and_fences_gc_and_retirement() {
        use crate::pickle::types::ImageCopyConfirmation;
        let mut inner = StateMachineInner::default();
        let commit = test_manifest_commit();
        inner.state.manifest_catalog.apply_manifest_commit(&commit);
        let mut copy = ImageCopyConfirmation {
            repository: commit.manifest.repository.clone(),
            manifest_digest: commit.manifest.digest.clone(),
            node_id: 3,
            lease_id: None,
            observed_gc_generation: 0,
            observed_at_unix_ms: 20,
        };
        let tags = inner.state.manifest_catalog.tags.clone();
        assert!(
            inner
                .apply_request(&RaftRequest::ConfirmImageCopy(copy.clone()))
                .is_none()
        );
        assert!(
            inner
                .apply_request(&RaftRequest::ConfirmImageCopy(copy.clone()))
                .is_none()
        );
        for digest in commit.manifest.referenced_digests() {
            assert_eq!(
                inner.state.manifest_catalog.layer_holders(digest.as_str()),
                std::collections::BTreeSet::from([1, 2, 3])
            );
        }
        assert_eq!(inner.state.manifest_catalog.tags, tags);
        copy.node_id = 4;
        inner.state.registry_gc_generations.insert(4, 1);
        assert_eq!(
            inner.apply_request(&RaftRequest::ConfirmImageCopy(copy.clone())),
            Some(CouncilResponse::RegistryPublicationStale)
        );
        copy.observed_gc_generation = 1;
        assert!(
            inner
                .apply_request(&RaftRequest::ConfirmImageCopy(copy.clone()))
                .is_none()
        );
        let valid = copy.clone();
        for (repository, digest, lease) in [
            ("unknown".to_owned(), copy.manifest_digest.clone(), None),
            (copy.repository.clone(), test_digest("unknown"), None),
            (
                copy.repository.clone(),
                copy.manifest_digest.clone(),
                Some("wrong".to_owned()),
            ),
        ] {
            let invalid = ImageCopyConfirmation {
                repository,
                manifest_digest: digest,
                lease_id: lease,
                ..valid.clone()
            };
            assert!(matches!(
                inner.apply_request(&RaftRequest::ConfirmImageCopy(invalid)),
                Some(CouncilResponse::Refused { .. })
            ));
        }
        let node = "retired-copy-node";
        copy.node_id = crate::cluster::identity::raft_id_from_name(node);
        copy.observed_gc_generation = 0;
        assert!(matches!(
            inner.apply_request(&RaftRequest::DecommissionNode {
                node_id: node.into(),
                retired_by: "operator".into(),
                reason: "isolated".into(),
                retired_at_unix_ms: 20,
                membership_log_id: None,
            }),
            Some(CouncilResponse::NodeDecommissioned { .. })
        ));
        assert!(matches!(
            inner.apply_request(&RaftRequest::ConfirmImageCopy(copy)),
            Some(CouncilResponse::Refused { .. })
        ));
    }

    #[test]
    fn leased_copy_confirmation_requires_exact_live_owner_and_writer_receipt() {
        use crate::pickle::types::ImageCopyConfirmation;
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::TestLeaseCreate(test_lease("run1", 100)));
        let mut commit = test_manifest_commit();
        commit.manifest.repository = "rbtest-run1/web".into();
        commit.holder_nodes = std::collections::BTreeSet::from([1]);
        for node_id in [1, 2] {
            assert!(
                inner
                    .apply_request(&RaftRequest::TestLeaseRegistryWriter {
                        lease_id: "run1".into(),
                        repository: commit.manifest.repository.clone(),
                        node_id,
                        owner_id: Some("token:ci".into()),
                        observed_at_unix_ms: 20,
                    })
                    .is_none()
            );
        }
        assert!(
            inner
                .apply_request(&RaftRequest::TestLeaseManifestCommit {
                    lease_id: "run1".into(),
                    observed_at_unix_ms: 20,
                    commit: Box::new(commit.clone()),
                })
                .is_none()
        );
        let copy = ImageCopyConfirmation {
            repository: commit.manifest.repository.clone(),
            manifest_digest: commit.manifest.digest.clone(),
            node_id: 2,
            lease_id: Some("run1".into()),
            observed_gc_generation: 0,
            observed_at_unix_ms: 20,
        };
        for invalid in [
            ImageCopyConfirmation {
                lease_id: None,
                ..copy.clone()
            },
            ImageCopyConfirmation {
                lease_id: Some("wrong".into()),
                ..copy.clone()
            },
            ImageCopyConfirmation {
                node_id: 3,
                ..copy.clone()
            },
            ImageCopyConfirmation {
                observed_at_unix_ms: 101,
                ..copy.clone()
            },
        ] {
            assert!(matches!(
                inner.apply_request(&RaftRequest::ConfirmImageCopy(invalid)),
                Some(CouncilResponse::Refused { .. })
            ));
        }
        assert!(
            inner
                .apply_request(&RaftRequest::ConfirmImageCopy(copy.clone()))
                .is_none()
        );
        inner.apply_request(&RaftRequest::TestLeaseBeginCleanup {
            lease_id: "run1".into(),
        });
        assert!(matches!(
            inner.apply_request(&RaftRequest::ConfirmImageCopy(copy.clone())),
            Some(CouncilResponse::Refused { .. })
        ));
        inner
            .state
            .manifest_catalog
            .retire_leased_repository(&copy.repository, "run1")
            .unwrap();
        assert!(matches!(
            inner.apply_request(&RaftRequest::ConfirmImageCopy(copy)),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(
            inner
                .state
                .manifest_catalog
                .get_repository_manifest(
                    &commit.manifest.repository,
                    commit.manifest.digest.as_str()
                )
                .is_none()
        );
    }

    #[test]
    fn unscoped_registry_holder_replacement_is_refused() {
        let mut inner = StateMachineInner::default();
        let commit = test_manifest_commit();
        inner.state.manifest_catalog.apply_manifest_commit(&commit);
        let digest = commit.manifest.digest;
        let before = inner.state.manifest_catalog.layer_holders(digest.as_str());
        let response = inner.apply_request(&RaftRequest::UpdateLayerLocations(
            crate::pickle::types::UpdateLayerLocations {
                updates: vec![(digest.clone(), std::collections::BTreeSet::from([99]))],
            },
        ));
        assert!(
            matches!(response, Some(CouncilResponse::Refused { .. })),
            "an old full holder set cannot establish present storage ownership"
        );
        assert_eq!(
            inner.state.manifest_catalog.layer_holders(digest.as_str()),
            before
        );
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
        sm.apply_fixture(vec![add, del]).await.unwrap();

        let state = sm.desired_state().await;
        assert!(state.apps.is_empty());
    }

    #[tokio::test]
    async fn app_delete_clears_a_stale_autoscale_override() {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "prod");
        let spec = AppSpec {
            replicas: crate::config::types::Replicas::Fixed(2),
            ..default_spec()
        };
        sm.apply_fixture(vec![
            normal_entry(
                1,
                1,
                RaftRequest::AppSpec {
                    app_id: app_id.clone(),
                    spec: Box::new(spec),
                },
            ),
            normal_entry(
                1,
                2,
                RaftRequest::AutoscaleOverride {
                    app_id: app_id.clone(),
                    replicas: 5,
                    reason: "load".to_string(),
                },
            ),
            normal_entry(
                1,
                3,
                RaftRequest::AppDelete {
                    app_id: app_id.clone(),
                },
            ),
        ])
        .await
        .unwrap();

        let state = sm.desired_state().await;
        assert!(
            state.autoscale_overrides.is_empty(),
            "deleting an app must clear its override"
        );
    }

    #[tokio::test]
    async fn baseline_change_clears_a_stale_autoscale_override() {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "prod");
        let spec2 = AppSpec {
            replicas: crate::config::types::Replicas::Fixed(2),
            ..default_spec()
        };
        let spec4 = AppSpec {
            replicas: crate::config::types::Replicas::Fixed(4),
            ..default_spec()
        };
        sm.apply_fixture(vec![
            normal_entry(
                1,
                1,
                RaftRequest::AppSpec {
                    app_id: app_id.clone(),
                    spec: Box::new(spec2),
                },
            ),
            normal_entry(
                1,
                2,
                RaftRequest::AutoscaleOverride {
                    app_id: app_id.clone(),
                    replicas: 5,
                    reason: "load".to_string(),
                },
            ),
            // Operator redeploys with a new baseline (2 → 4): the old
            // override must not survive to resize the app.
            normal_entry(
                1,
                3,
                RaftRequest::AppSpec {
                    app_id: app_id.clone(),
                    spec: Box::new(spec4),
                },
            ),
        ])
        .await
        .unwrap();

        let state = sm.desired_state().await;
        assert!(
            state.autoscale_overrides.is_empty(),
            "a changed replica baseline must clear the stale override"
        );
    }

    #[tokio::test]
    async fn same_baseline_redeploy_keeps_the_override() {
        // Redeploying with the SAME replica baseline (e.g. an image bump)
        // must not disturb a live autoscale override.
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "prod");
        let spec = AppSpec {
            replicas: crate::config::types::Replicas::Fixed(2),
            image: Some("web:v1".to_string()),
            ..default_spec()
        };
        let spec_new_image = AppSpec {
            replicas: crate::config::types::Replicas::Fixed(2),
            image: Some("web:v2".to_string()),
            ..default_spec()
        };
        sm.apply_fixture(vec![
            normal_entry(
                1,
                1,
                RaftRequest::AppSpec {
                    app_id: app_id.clone(),
                    spec: Box::new(spec),
                },
            ),
            normal_entry(
                1,
                2,
                RaftRequest::AutoscaleOverride {
                    app_id: app_id.clone(),
                    replicas: 5,
                    reason: "load".to_string(),
                },
            ),
            normal_entry(
                1,
                3,
                RaftRequest::AppSpec {
                    app_id: app_id.clone(),
                    spec: Box::new(spec_new_image),
                },
            ),
        ])
        .await
        .unwrap();

        let state = sm.desired_state().await;
        assert_eq!(
            state.autoscale_overrides,
            vec![("prod/web".to_string(), 5)],
            "an image-only redeploy must keep the override"
        );
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
                    ordinal: 0,
                },
                Placement {
                    node_id: NodeId::new("node-2"),
                    resources: Resources::new(500, 256 * 1024 * 1024, 0),
                    ordinal: 1,
                },
            ],
        };
        let entry = normal_entry(1, 1, RaftRequest::SchedulingDecision(decision));
        sm.apply_fixture(vec![entry]).await.unwrap();

        let state = sm.desired_state().await;
        let placements = state.scheduling.get(&app_id).unwrap();
        assert_eq!(placements.len(), 2);
    }

    fn withdrawal_fixture_catalogue() -> crate::onion::catalog::EndpointCatalog {
        use crate::onion::catalog::{CatalogBackend, EndpointCatalog};
        use crate::onion::service_id::ServiceId;
        EndpointCatalog::rebuild([(
            ServiceId::new("default", "api"),
            80,
            vec![CatalogBackend {
                node_id: "producer".into(),
                node_ip: "10.0.0.1".parse().unwrap(),
                host_port: 30001,
                healthy: true,
                execution: Some(crate::grill::RuntimeExecution {
                    instance_id: crate::grill::InstanceId("default__api-0".into()),
                    generation: "a".repeat(64).try_into().unwrap(),
                }),
            }],
        )])
        .unwrap()
    }

    fn retire_endpoint(node_id: &str, execution: &crate::grill::RuntimeExecution) -> RaftRequest {
        serde_json::from_value(serde_json::json!({"RetireEndpointExecution": {
            "node_id": node_id, "execution": execution
        }}))
        .unwrap()
    }

    fn released(response: Option<CouncilResponse>, expected: bool) {
        assert_eq!(
            serde_json::to_value(response.unwrap()).unwrap(),
            serde_json::json!({"EndpointExecutionRetired": {"released": expected}})
        );
    }

    #[test]
    fn producer_retirement_fences_reports_and_waits_for_every_original_consumer() {
        let mut inner = StateMachineInner::default();
        inner
            .state
            .endpoint_consumers
            .extend(["reader".into(), "offline".into()]);
        let first = withdrawal_fixture_catalogue();
        let execution = first.services["default__api"].backends[0]
            .execution
            .clone()
            .unwrap();
        assert!(
            inner
                .apply_request(&RaftRequest::PublishEndpoints {
                    expected_generation: 0,
                    catalog: Box::new(first.clone())
                })
                .is_none()
        );
        let request = retire_endpoint("producer", &execution);
        released(inner.apply_request(&request), false);
        assert!(
            inner.state.endpoint_catalog.services["default__api"]
                .backends
                .is_empty()
        );
        assert_eq!(inner.state.endpoint_withdrawals.generation, 2);
        let before = serde_json::to_value(&inner.state).unwrap();
        released(inner.apply_request(&request), false);
        assert_eq!(serde_json::to_value(&inner.state).unwrap(), before);
        for unknown in [false, true] {
            let mut stale = first.clone();
            if unknown {
                stale.services.get_mut("default__api").unwrap().backends[0].execution = None;
            }
            assert!(matches!(
                inner.apply_request(&RaftRequest::PublishEndpoints {
                    expected_generation: 2,
                    catalog: Box::new(stale)
                }),
                Some(CouncilResponse::Refused { .. })
            ));
            assert_eq!(serde_json::to_value(&inner.state).unwrap(), before);
        }
        assert!(
            inner
                .apply_request(&endpoint_receipt("reader", 1))
                .is_none()
        );
        released(inner.apply_request(&request), false);
        assert!(matches!(
            inner.apply_request(&RaftRequest::DecommissionNode {
                node_id: "offline".into(),
                retired_by: "operator".into(),
                reason: "fenced".into(),
                retired_at_unix_ms: 1,
                membership_log_id: None,
            }),
            Some(CouncilResponse::NodeDecommissioned { .. })
        ));
        released(inner.apply_request(&request), true);
        let mut successor = first.clone();
        successor.services.get_mut("default__api").unwrap().backends[0]
            .execution
            .as_mut()
            .unwrap()
            .generation = "b".repeat(64).try_into().unwrap();
        assert!(
            inner
                .apply_request(&RaftRequest::PublishEndpoints {
                    expected_generation: 2,
                    catalog: Box::new(successor.clone())
                })
                .is_none()
        );
        released(inner.apply_request(&request), true);
        assert_eq!(inner.state.endpoint_catalog, successor);
        assert!(matches!(
            inner.apply_request(&RaftRequest::PublishEndpoints {
                expected_generation: 3,
                catalog: Box::new(first)
            }),
            Some(CouncilResponse::Refused { .. })
        ));
    }

    #[test]
    fn producer_retirement_checks_all_historical_exposures_and_refuses_atomically_at_capacity() {
        let mut inner = StateMachineInner::default();
        inner.state.endpoint_consumers.insert("reader".into());
        let mut catalog = withdrawal_fixture_catalogue();
        let execution = catalog.services["default__api"].backends[0]
            .execution
            .clone()
            .unwrap();
        for generation in 0..3 {
            catalog.services.get_mut("default__api").unwrap().backends[0].host_port += 1;
            assert!(
                inner
                    .apply_request(&RaftRequest::PublishEndpoints {
                        expected_generation: generation,
                        catalog: Box::new(catalog.clone())
                    })
                    .is_none()
            );
        }
        let request = retire_endpoint("producer", &execution);
        released(inner.apply_request(&request), false);
        for generation in [3, 1] {
            inner.apply_request(&endpoint_receipt("reader", generation));
            released(inner.apply_request(&request), false);
        }
        inner.apply_request(&endpoint_receipt("reader", 2));
        released(inner.apply_request(&request), true);

        let mut full = StateMachineInner::default();
        let mut wire = serde_json::to_value(&full.state).unwrap();
        let entries: serde_json::Map<String, serde_json::Value> = (0..65_536)
            .map(|n| (format!("{n:064x}"), serde_json::json!("default__api-0")))
            .collect();
        wire["producer_retirements"] = serde_json::json!({"executions": {"producer": entries}});
        full.state = serde_json::from_value(wire).unwrap();
        let before = serde_json::to_value(&full.state).unwrap();
        assert!(matches!(
            full.apply_request(&request),
            Some(CouncilResponse::Refused { .. })
        ));
        assert_eq!(serde_json::to_value(&full.state).unwrap(), before);
    }

    #[test]
    fn producer_retirement_withdrawal_failure_preserves_catalogue_fence_and_history() {
        let mut inner = StateMachineInner::default();
        inner.state.endpoint_catalog = withdrawal_fixture_catalogue();
        inner.state.endpoint_withdrawals.generation = u64::MAX;
        let execution = inner.state.endpoint_catalog.services["default__api"].backends[0]
            .execution
            .clone()
            .unwrap();
        let before = serde_json::to_value(&inner.state).unwrap();
        assert!(matches!(
            inner.apply_request(&retire_endpoint("producer", &execution)),
            Some(CouncilResponse::Refused { .. })
        ));
        assert_eq!(serde_json::to_value(&inner.state).unwrap(), before);
    }

    #[tokio::test]
    async fn producer_retirement_and_delayed_report_fences_survive_raft_snapshot() {
        let mut sm = CouncilStateMachine::new();
        let catalog = withdrawal_fixture_catalogue();
        let execution = catalog.services["default__api"].backends[0]
            .execution
            .clone()
            .unwrap();
        let retirement = retire_endpoint("producer", &execution);
        // Never-published executions must also acquire a fence before addresses can be reused.
        let responses = sm
            .apply_fixture(vec![normal_entry(1, 1, retirement.clone())])
            .await
            .unwrap();
        released(Some(responses[0].clone()), true);
        let mut builder = sm.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.unwrap();
        let mut restored = CouncilStateMachine::new();
        restored
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        let responses = restored
            .apply_fixture(vec![
                normal_entry(
                    1,
                    2,
                    RaftRequest::PublishEndpoints {
                        expected_generation: 0,
                        catalog: Box::new(catalog),
                    },
                ),
                normal_entry(1, 3, retirement),
            ])
            .await
            .unwrap();
        assert!(matches!(responses[0], CouncilResponse::Refused { .. }));
        released(Some(responses[1].clone()), true);
    }

    fn endpoint_receipt(node_id: &str, generation: u64) -> RaftRequest {
        serde_json::from_value(serde_json::json!({
            "AcknowledgeEndpointWithdrawal": {"node_id": node_id, "generation": generation}
        }))
        .unwrap()
    }

    #[test]
    fn endpoint_receipts_refuse_invalid_identities_and_nonhistorical_generations_atomically() {
        let mut inner = StateMachineInner::default();
        inner.state.endpoint_consumers.insert("reader".into());
        let first = withdrawal_fixture_catalogue();
        for (expected_generation, catalog) in [first, Default::default()].into_iter().enumerate() {
            assert!(
                inner
                    .apply_request(&RaftRequest::PublishEndpoints {
                        expected_generation: expected_generation as u64,
                        catalog: Box::new(catalog),
                    })
                    .is_none()
            );
        }
        assert!(matches!(
            inner.apply_request(&RaftRequest::DecommissionNode {
                node_id: "retired".into(),
                retired_by: "operator".into(),
                reason: "fenced".into(),
                retired_at_unix_ms: 1,
                membership_log_id: None,
            }),
            Some(CouncilResponse::NodeDecommissioned { .. })
        ));
        let original = serde_json::to_value(&inner.state).unwrap();
        for (node, generation) in [
            ("reader", 0),
            ("reader", 2),
            ("reader", u64::MAX),
            ("unregistered", 1),
            ("retired", 1),
            ("../reader", 1),
        ] {
            assert!(
                matches!(
                    inner.apply_request(&endpoint_receipt(node, generation)),
                    Some(CouncilResponse::Refused { .. })
                ),
                "{node}/{generation} must refuse"
            );
            assert_eq!(serde_json::to_value(&inner.state).unwrap(), original);
        }
    }

    #[tokio::test]
    async fn endpoint_receipts_survive_snapshot_and_replay_without_discharging_other_consumers() {
        let mut sm = CouncilStateMachine::new();
        let requests = [
            RaftRequest::RegisterEndpointConsumer {
                node_id: "reader".into(),
            },
            RaftRequest::RegisterEndpointConsumer {
                node_id: "offline".into(),
            },
            RaftRequest::PublishEndpoints {
                expected_generation: 0,
                catalog: Box::new(withdrawal_fixture_catalogue()),
            },
            RaftRequest::PublishEndpoints {
                expected_generation: 1,
                catalog: Box::default(),
            },
            endpoint_receipt("reader", 1),
        ];
        for (index, request) in requests.into_iter().enumerate() {
            let response = sm
                .apply_fixture(vec![normal_entry(1, index as u64 + 1, request)])
                .await
                .unwrap();
            assert!(matches!(response[0], CouncilResponse::Applied { .. }));
        }
        let before = sm.desired_state().await;
        assert_eq!(
            before.endpoint_withdrawals.pending[&1].consumers,
            std::collections::BTreeSet::from(["offline".into()])
        );
        let mut builder = sm.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.unwrap();
        let mut restored = CouncilStateMachine::new();
        restored
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        let response = restored
            .apply_fixture(vec![normal_entry(2, 6, endpoint_receipt("reader", 1))])
            .await
            .unwrap();
        assert!(matches!(response[0], CouncilResponse::Applied { .. }));
        let replayed = restored.desired_state().await;
        assert_eq!(replayed.endpoint_withdrawals, before.endpoint_withdrawals);
        assert_eq!(replayed.endpoint_consumers, before.endpoint_consumers);
        let response = restored
            .apply_fixture(vec![normal_entry(2, 7, endpoint_receipt("offline", 1))])
            .await
            .unwrap();
        assert!(matches!(response[0], CouncilResponse::Applied { .. }));
        let after = restored.desired_state().await;
        assert!(after.endpoint_withdrawals.pending.is_empty());
        assert_eq!(after.endpoint_withdrawals.generation, 2);
        assert_eq!(after.endpoint_consumers, before.endpoint_consumers);
        assert!(after.endpoint_catalog.is_empty());
    }

    #[test]
    fn endpoint_publication_generation_rejects_old_and_future_writers_atomically() {
        let mut inner = StateMachineInner::default();
        inner.state.endpoint_consumers.insert("reader".into());
        let first = withdrawal_fixture_catalogue();
        assert!(
            inner
                .apply_request(&RaftRequest::PublishEndpoints {
                    expected_generation: 0,
                    catalog: Box::new(first.clone())
                })
                .is_none()
        );
        let mut second = first.clone();
        second.services.get_mut("default__api").unwrap().backends[0].host_port = 30002;
        assert!(
            inner
                .apply_request(&RaftRequest::PublishEndpoints {
                    expected_generation: 1,
                    catalog: Box::new(second.clone())
                })
                .is_none()
        );
        assert_eq!(inner.state.endpoint_withdrawals.pending.len(), 1);
        let before = serde_json::to_value(&inner.state).unwrap();
        let mut candidate = second;
        candidate.services.get_mut("default__api").unwrap().backends[0].host_port = 30003;
        for expected_generation in [1, 3] {
            assert!(matches!(
                inner.apply_request(&RaftRequest::PublishEndpoints {
                    expected_generation,
                    catalog: Box::new(candidate.clone())
                }),
                Some(CouncilResponse::Refused { .. })
            ));
            assert_eq!(serde_json::to_value(&inner.state).unwrap(), before);
        }
    }

    #[test]
    fn endpoint_publication_generation_requires_current_evidence_even_for_noops() {
        let mut inner = StateMachineInner::default();
        assert!(matches!(
            inner.apply_request(&RaftRequest::PublishEndpoints {
                expected_generation: 1,
                catalog: Box::default()
            }),
            Some(CouncilResponse::Refused { .. })
        ));
        let first = withdrawal_fixture_catalogue();
        let request = RaftRequest::PublishEndpoints {
            expected_generation: 0,
            catalog: Box::new(first.clone()),
        };
        assert!(inner.apply_request(&request).is_none());
        let before = serde_json::to_value(&inner.state).unwrap();
        assert!(matches!(
            inner.apply_request(&request),
            Some(CouncilResponse::Refused { .. })
        ));
        assert_eq!(serde_json::to_value(&inner.state).unwrap(), before);
        assert!(
            inner
                .apply_request(&RaftRequest::PublishEndpoints {
                    expected_generation: 1,
                    catalog: Box::new(first)
                })
                .is_none()
        );
        assert_eq!(serde_json::to_value(&inner.state).unwrap(), before);
    }

    #[tokio::test]
    async fn endpoint_publication_generation_survives_snapshot_for_stale_noop_refusal() {
        let mut sm = CouncilStateMachine::new();
        let request = RaftRequest::PublishEndpoints {
            expected_generation: 0,
            catalog: Box::new(withdrawal_fixture_catalogue()),
        };
        let applied = sm
            .apply_fixture(vec![normal_entry(1, 1, request.clone())])
            .await
            .unwrap();
        assert!(matches!(applied[0], CouncilResponse::Applied { .. }));
        let original = sm.desired_state().await;
        let mut builder = sm.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.unwrap();
        let mut restored = CouncilStateMachine::new();
        restored
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        let response = restored
            .apply_fixture(vec![normal_entry(2, 2, request)])
            .await
            .unwrap();
        assert!(matches!(response[0], CouncilResponse::Refused { .. }));
        let state = restored.desired_state().await;
        assert_eq!(state.endpoint_catalog, original.endpoint_catalog);
        assert_eq!(state.endpoint_withdrawals, original.endpoint_withdrawals);
    }

    #[test]
    fn endpoint_publication_generation_observes_decommission_changes() {
        let mut inner = StateMachineInner::default();
        inner.state.endpoint_consumers.insert("reader".into());
        let first = withdrawal_fixture_catalogue();
        assert!(
            inner
                .apply_request(&RaftRequest::PublishEndpoints {
                    expected_generation: 0,
                    catalog: Box::new(first.clone())
                })
                .is_none()
        );
        let request = RaftRequest::DecommissionNode {
            node_id: "producer".into(),
            retired_by: "operator".into(),
            reason: "powered off".into(),
            retired_at_unix_ms: 10,
            membership_log_id: None,
        };
        assert!(matches!(
            inner.apply_request(&request),
            Some(CouncilResponse::NodeDecommissioned { .. })
        ));
        assert_eq!(inner.state.endpoint_withdrawals.generation, 2);
        let mut candidate = first;
        let backend = &mut candidate.services.get_mut("default__api").unwrap().backends[0];
        backend.node_id = "replacement".into();
        backend.node_ip = "10.0.0.2".parse().unwrap();
        let before = serde_json::to_value(&inner.state).unwrap();
        assert!(matches!(
            inner.apply_request(&RaftRequest::PublishEndpoints {
                expected_generation: 1,
                catalog: Box::new(candidate.clone())
            }),
            Some(CouncilResponse::Refused { .. })
        ));
        assert_eq!(serde_json::to_value(&inner.state).unwrap(), before);
        assert!(
            inner
                .apply_request(&RaftRequest::PublishEndpoints {
                    expected_generation: 2,
                    catalog: Box::new(candidate)
                })
                .is_none()
        );
        assert_eq!(inner.state.endpoint_withdrawals.generation, 3);
        assert_eq!(
            inner.state.endpoint_withdrawals.pending,
            serde_json::from_value::<DesiredState>(before)
                .unwrap()
                .endpoint_withdrawals
                .pending
        );
    }

    #[tokio::test]
    async fn endpoint_withdrawals_survive_raft_snapshot_and_fenced_consumer_retirement() {
        let mut sm = CouncilStateMachine::new();
        let catalogue = withdrawal_fixture_catalogue();
        let requests = [
            RaftRequest::RegisterEndpointConsumer {
                node_id: "offline".into(),
            },
            RaftRequest::RegisterEndpointConsumer {
                node_id: "survivor".into(),
            },
            RaftRequest::PublishEndpoints {
                expected_generation: 0,
                catalog: Box::new(catalogue.clone()),
            },
            RaftRequest::PublishEndpoints {
                expected_generation: 1,
                catalog: Box::default(),
            },
            RaftRequest::RegisterEndpointConsumer {
                node_id: "late-reader".into(),
            },
        ];
        for (index, request) in requests.into_iter().enumerate() {
            let responses = sm
                .apply_fixture(vec![normal_entry(1, index as u64 + 1, request)])
                .await
                .unwrap();
            assert!(matches!(responses[0], CouncilResponse::Applied { .. }));
        }
        let original = sm.desired_state().await.endpoint_withdrawals;
        assert_eq!(original.generation, 2);
        assert_eq!(
            original.pending[&1].consumers,
            std::collections::BTreeSet::from(["offline".into(), "survivor".into()])
        );
        assert_eq!(
            original.pending[&1].services["default__api"].service,
            catalogue.services["default__api"]
        );
        let mut builder = sm.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.unwrap();
        let mut restored = CouncilStateMachine::new();
        restored
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        assert_eq!(
            restored.desired_state().await.endpoint_withdrawals,
            original
        );
        let request = RaftRequest::DecommissionNode {
            node_id: "offline".into(),
            retired_by: "operator".into(),
            reason: "powered off".into(),
            retired_at_unix_ms: 10,
            membership_log_id: None,
        };
        let first = restored
            .apply_fixture(vec![normal_entry(1, 6, request.clone())])
            .await
            .unwrap();
        assert!(matches!(
            first[0],
            CouncilResponse::NodeDecommissioned { .. }
        ));
        let after = restored.desired_state().await.endpoint_withdrawals;
        assert_eq!(
            after.pending[&1].consumers,
            std::collections::BTreeSet::from(["survivor".into()])
        );
        assert_eq!(after.generation, 2);
        assert_eq!(
            restored
                .apply_fixture(vec![normal_entry(1, 7, request)])
                .await
                .unwrap(),
            first
        );
        assert_eq!(restored.desired_state().await.endpoint_withdrawals, after);
    }

    #[test]
    fn endpoint_withdrawals_on_decommission_retain_other_consumers_of_the_fenced_producer() {
        let mut inner = StateMachineInner::default();
        for node in ["producer", "reader"] {
            inner.apply_request(&RaftRequest::RegisterEndpointConsumer {
                node_id: node.into(),
            });
        }
        let catalogue = withdrawal_fixture_catalogue();
        inner.apply_request(&RaftRequest::PublishEndpoints {
            expected_generation: 0,
            catalog: Box::new(catalogue.clone()),
        });
        let request = RaftRequest::DecommissionNode {
            node_id: "producer".into(),
            retired_by: "operator".into(),
            reason: "powered off".into(),
            retired_at_unix_ms: 10,
            membership_log_id: None,
        };
        assert!(matches!(
            inner.apply_request(&request),
            Some(CouncilResponse::NodeDecommissioned { .. })
        ));
        let pending = &inner.state.endpoint_withdrawals.pending[&1];
        assert_eq!(
            pending.consumers,
            std::collections::BTreeSet::from(["reader".into()])
        );
        assert_eq!(
            pending.services["default__api"].service,
            catalogue.services["default__api"]
        );
        assert!(!pending.services["default__api"].retire_vip);
        assert!(
            inner.state.endpoint_catalog.services["default__api"]
                .backends
                .is_empty()
        );
    }

    #[test]
    fn retired_vip_publication_is_refused_without_changing_committed_state() {
        let mut inner = StateMachineInner::default();
        inner.state.endpoint_consumers.insert("reader".into());
        let original = withdrawal_fixture_catalogue();
        inner.apply_request(&RaftRequest::PublishEndpoints {
            expected_generation: 0,
            catalog: Box::new(original.clone()),
        });
        inner.apply_request(&RaftRequest::PublishEndpoints {
            expected_generation: 1,
            catalog: Box::default(),
        });
        let before = serde_json::to_value(&inner.state).unwrap();
        assert!(matches!(
            inner.apply_request(&RaftRequest::PublishEndpoints {
                expected_generation: 2,
                catalog: Box::new(original)
            }),
            Some(CouncilResponse::Refused { .. })
        ));
        assert_eq!(serde_json::to_value(&inner.state).unwrap(), before);
    }

    #[test]
    fn endpoint_withdrawal_refusal_leaves_publication_and_decommission_atomic() {
        let mut inner = StateMachineInner::default();
        inner.state.endpoint_consumers.insert("reader".into());
        inner.state.endpoint_catalog = withdrawal_fixture_catalogue();
        inner.state.endpoint_withdrawals.generation = u64::MAX;
        let before = serde_json::to_value(&inner.state).unwrap();
        for request in [
            RaftRequest::PublishEndpoints {
                expected_generation: u64::MAX,
                catalog: Box::default(),
            },
            RaftRequest::DecommissionNode {
                node_id: "producer".into(),
                retired_by: "operator".into(),
                reason: "powered off".into(),
                retired_at_unix_ms: 10,
                membership_log_id: None,
            },
        ] {
            assert!(matches!(
                inner.apply_request(&request),
                Some(CouncilResponse::Refused { .. })
            ));
            assert_eq!(serde_json::to_value(&inner.state).unwrap(), before);
        }
    }

    /// Z6.7: a stopped laptop node was still owed every withdrawal, so no
    /// producer on the surviving nodes could release an address until it came
    /// back. A lapsed consumer's discharge frees exactly its own obligations.
    #[test]
    fn discharging_a_lapsed_consumer_releases_only_its_confirmations() {
        let mut inner = StateMachineInner::default();
        for node in ["stopped", "survivor"] {
            inner.apply_request(&RaftRequest::RegisterEndpointConsumer {
                node_id: node.into(),
            });
        }
        let original = withdrawal_fixture_catalogue();
        inner.apply_request(&RaftRequest::PublishEndpoints {
            expected_generation: 0,
            catalog: Box::new(original),
        });
        inner.apply_request(&RaftRequest::PublishEndpoints {
            expected_generation: 1,
            catalog: Box::default(),
        });
        assert_eq!(
            inner.state.endpoint_withdrawals.pending[&1].consumers,
            std::collections::BTreeSet::from(["stopped".into(), "survivor".into()])
        );

        assert!(matches!(
            inner.apply_request(&RaftRequest::DischargeEndpointConsumer {
                node_id: "stopped".into(),
            }),
            None | Some(CouncilResponse::Applied { .. })
        ));
        assert_eq!(
            inner.state.endpoint_consumers,
            std::collections::BTreeSet::from(["survivor".into()])
        );
        assert_eq!(
            inner.state.endpoint_withdrawals.pending[&1].consumers,
            std::collections::BTreeSet::from(["survivor".into()]),
            "the survivor still owes its own receipt"
        );
        // Discharge is not retirement: the node may come back and register.
        assert!(
            !inner
                .state
                .security_state
                .crl
                .retired_nodes
                .contains_key("stopped")
        );
        assert!(!matches!(
            inner.apply_request(&RaftRequest::RegisterEndpointConsumer {
                node_id: "stopped".into(),
            }),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(inner.state.endpoint_consumers.contains("stopped"));
        assert!(
            !inner.state.endpoint_withdrawals.pending[&1]
                .consumers
                .contains("stopped"),
            "re-registering doesn't resurrect a discharged obligation"
        );

        // The last consumer's discharge completes the generation.
        inner.apply_request(&RaftRequest::DischargeEndpointConsumer {
            node_id: "survivor".into(),
        });
        assert!(inner.state.endpoint_withdrawals.pending.is_empty());
        // Unknown and invalid identities change nothing.
        let before = serde_json::to_value(&inner.state).unwrap();
        inner.apply_request(&RaftRequest::DischargeEndpointConsumer {
            node_id: "never-registered".into(),
        });
        assert!(matches!(
            inner.apply_request(&RaftRequest::DischargeEndpointConsumer {
                node_id: String::new(),
            }),
            Some(CouncilResponse::Refused { .. })
        ));
        assert_eq!(serde_json::to_value(&inner.state).unwrap(), before);
    }

    #[tokio::test]
    async fn endpoint_consumers_survive_snapshot_and_require_explicit_decommission() {
        let mut sm = CouncilStateMachine::new();
        for (index, node) in ["offline", "survivor", "offline"].into_iter().enumerate() {
            let responses = sm
                .apply_fixture(vec![normal_entry(
                    1,
                    index as u64 + 1,
                    RaftRequest::RegisterEndpointConsumer {
                        node_id: node.into(),
                    },
                )])
                .await
                .unwrap();
            assert!(matches!(responses[0], CouncilResponse::Applied { .. }));
        }
        let mut builder = sm.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.unwrap();
        let mut restored = CouncilStateMachine::new();
        restored
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        assert_eq!(
            restored.desired_state().await.endpoint_consumers,
            std::collections::BTreeSet::from(["offline".into(), "survivor".into()])
        );
        let request = RaftRequest::DecommissionNode {
            node_id: "offline".into(),
            retired_by: "operator".into(),
            reason: "powered off".into(),
            retired_at_unix_ms: 10,
            membership_log_id: None,
        };
        let response = restored
            .apply_fixture(vec![normal_entry(1, 4, request.clone())])
            .await
            .unwrap();
        assert!(matches!(
            response[0],
            CouncilResponse::NodeDecommissioned { .. }
        ));
        assert_eq!(
            restored.desired_state().await.endpoint_consumers,
            std::collections::BTreeSet::from(["survivor".into()])
        );
        let repeated = restored
            .apply_fixture(vec![normal_entry(1, 5, request)])
            .await
            .unwrap();
        assert_eq!(repeated, response);
        assert!(
            restored
                .desired_state()
                .await
                .security_state
                .crl
                .retired_nodes["offline"]
                .released_endpoint_consumer
        );
        let refused = restored
            .apply_fixture(vec![normal_entry(
                1,
                6,
                RaftRequest::RegisterEndpointConsumer {
                    node_id: "offline".into(),
                },
            )])
            .await
            .unwrap();
        assert!(matches!(refused[0], CouncilResponse::Refused { .. }));
    }

    #[test]
    fn endpoint_consumer_registration_is_bounded_and_never_evicts_existing_owners() {
        let mut inner = StateMachineInner::default();
        for node in ["", "invalid\nidentity"] {
            assert!(matches!(
                inner.apply_request(&RaftRequest::RegisterEndpointConsumer {
                    node_id: node.into()
                }),
                Some(CouncilResponse::Refused { .. })
            ));
        }
        inner.state.endpoint_consumers = (0..65_536).map(|n| format!("worker-{n}")).collect();
        assert!(matches!(
            inner.apply_request(&RaftRequest::RegisterEndpointConsumer {
                node_id: "overflow".into()
            }),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(
            inner
                .apply_request(&RaftRequest::RegisterEndpointConsumer {
                    node_id: "worker-0".into()
                })
                .is_none()
        );
        assert_eq!(inner.state.endpoint_consumers.len(), 65_536);
        assert!(!inner.state.endpoint_consumers.contains("overflow"));
    }

    #[tokio::test]
    async fn apply_publish_endpoints_replaces_catalogue() {
        use crate::onion::catalog::{CatalogBackend, EndpointCatalog};
        use crate::onion::service_id::ServiceId;

        let mut sm = CouncilStateMachine::new();
        // Two same-named apps in different namespaces must both land in the
        // replicated catalogue, with distinct VIPs (the D3 fix, cluster-wide).
        let catalog = EndpointCatalog::rebuild([
            (
                ServiceId::new("default", "api"),
                3000,
                vec![CatalogBackend {
                    execution: None,
                    node_id: "node-a".to_string(),
                    node_ip: std::net::Ipv4Addr::new(10, 0, 0, 1),
                    host_port: 30001,
                    healthy: true,
                }],
            ),
            (
                ServiceId::new("payments", "api"),
                3000,
                vec![CatalogBackend {
                    execution: None,
                    node_id: "node-b".to_string(),
                    node_ip: std::net::Ipv4Addr::new(10, 0, 0, 2),
                    host_port: 30002,
                    healthy: true,
                }],
            ),
        ])
        .unwrap();
        let entry = normal_entry(
            1,
            1,
            RaftRequest::PublishEndpoints {
                expected_generation: 0,
                catalog: Box::new(catalog),
            },
        );
        sm.apply_fixture(vec![entry]).await.unwrap();

        let state = sm.desired_state().await;
        let d = state
            .endpoint_catalog
            .resolve(&ServiceId::new("default", "api"))
            .unwrap();
        let p = state
            .endpoint_catalog
            .resolve(&ServiceId::new("payments", "api"))
            .unwrap();
        assert_ne!(
            d.vip, p.vip,
            "cluster catalogue shared a VIP across namespaces"
        );
        assert_eq!(d.backends[0].node_id, "node-a");
        assert_eq!(p.backends[0].node_id, "node-b");

        // A later publish wholly replaces the catalogue (leader is authoritative).
        let replacement =
            EndpointCatalog::rebuild([(ServiceId::new("default", "web"), 80, vec![])]).unwrap();
        let entry2 = normal_entry(
            2,
            1,
            RaftRequest::PublishEndpoints {
                expected_generation: 1,
                catalog: Box::new(replacement),
            },
        );
        sm.apply_fixture(vec![entry2]).await.unwrap();
        let state = sm.desired_state().await;
        assert!(
            state
                .endpoint_catalog
                .resolve(&ServiceId::new("default", "api"))
                .is_none(),
            "stale service survived a wholesale republish"
        );
        assert!(
            state
                .endpoint_catalog
                .resolve(&ServiceId::new("default", "web"))
                .is_some()
        );
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
        sm.apply_fixture(vec![entry]).await.unwrap();

        let state = sm.desired_state().await;
        assert_eq!(state.config.get("max_apps").unwrap(), "100");
    }

    #[tokio::test]
    async fn apply_noop_changes_nothing() {
        let mut sm = CouncilStateMachine::new();
        let entry = normal_entry(1, 1, RaftRequest::Noop);
        let responses = sm.apply_fixture(vec![entry]).await.unwrap();
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
        sm.apply_fixture(vec![entry]).await.unwrap();

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
        sm.apply_fixture(entries).await.unwrap();

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

        let responses = sm.apply_fixture(vec![entry]).await.unwrap();
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

    /// #426: a learner joining after the snapshot was built must be told
    /// the log position the snapshot holds, not where the live state has
    /// moved on to since. Otherwise it records entries it never received.
    #[tokio::test]
    async fn current_snapshot_describes_its_own_contents_not_the_live_state() {
        let mut sm = CouncilStateMachine::new();
        sm.apply_fixture(vec![normal_entry(
            1,
            1,
            RaftRequest::ConfigSet {
                key: "in-snapshot".to_string(),
                value: "yes".to_string(),
            },
        )])
        .await
        .unwrap();
        let mut builder = sm.get_snapshot_builder().await;
        builder.build_snapshot().await.unwrap();

        // The live state moves on past the snapshot.
        sm.apply_fixture(vec![normal_entry(
            1,
            2,
            RaftRequest::ConfigSet {
                key: "after-snapshot".to_string(),
                value: "yes".to_string(),
            },
        )])
        .await
        .unwrap();

        let current = sm.get_current_snapshot().await.unwrap().unwrap();
        assert_eq!(current.meta.last_log_id, Some(log_id(1, 1)));
        let mut data = Vec::new();
        let mut cursor = *current.snapshot;
        cursor.read_to_end(&mut data).unwrap();
        let held: DesiredState = serde_json::from_slice(&data).unwrap();
        assert!(held.config.contains_key("in-snapshot"));
        assert!(!held.config.contains_key("after-snapshot"));
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
        sm.apply_fixture(vec![entry]).await.unwrap();

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
            observed_gc_generation: 0,
            manifest: crate::pickle::types::ImageManifest {
                digest: test_digest("m1"),
                config: crate::pickle::types::LayerDescriptor {
                    digest: test_digest("cfg"),
                    size: 512,
                    media_type: String::new(),
                    platform: None,
                },
                layers: vec![crate::pickle::types::LayerDescriptor {
                    digest: test_digest("layer1"),
                    size: 4096,
                    media_type: String::new(),
                    platform: None,
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

    #[test]
    fn a_delayed_manifest_cannot_publish_across_a_gc_generation() {
        let mut inner = StateMachineInner::default();
        let report = crate::pickle::types::GcReport {
            node_id: 1,
            deleted_layers: vec![test_digest("orphan")],
        };
        assert!(
            matches!(inner.apply_request(&RaftRequest::GcReport(report)), Some(CouncilResponse::GcApproved { approved }) if approved.len() == 1)
        );
        let mut commit = serde_json::to_value(test_manifest_commit()).unwrap();
        commit["holder_nodes"] = serde_json::json!([1]);
        commit["observed_gc_generation"] = serde_json::json!(0);
        let request: RaftRequest =
            serde_json::from_value(serde_json::json!({ "ManifestCommit": commit })).unwrap();
        assert!(
            matches!(
                inner.apply_request(&request),
                Some(CouncilResponse::RegistryPublicationStale)
            ),
            "a publication verified before GC must not restore its old holdings"
        );
        assert!(inner.state.manifest_catalog.manifests.is_empty());
    }

    #[test]
    fn exhausted_gc_generation_refuses_without_mutating_the_catalogue() {
        let mut inner = StateMachineInner::default();
        let mut encoded = serde_json::to_value(&inner.state).unwrap();
        encoded["registry_gc_generations"] = serde_json::json!({"1": u64::MAX});
        inner.state = serde_json::from_value(encoded).unwrap();
        let before = serde_json::to_value(&inner.state.manifest_catalog).unwrap();
        let report = crate::pickle::types::GcReport {
            node_id: 1,
            deleted_layers: vec![test_digest("orphan")],
        };
        assert!(
            matches!(
                inner.apply_request(&RaftRequest::GcReport(report)),
                Some(CouncilResponse::Refused { .. })
            ),
            "collection needs a fresh fencing generation before it can delete bytes"
        );
        assert_eq!(
            serde_json::to_value(&inner.state.manifest_catalog).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn gc_generation_survives_snapshot_and_only_advances_for_approved_deletions() {
        let mut sm = CouncilStateMachine::new();
        sm.apply_fixture(vec![normal_entry(
            1,
            1,
            RaftRequest::GcReport(crate::pickle::types::GcReport {
                node_id: 1,
                deleted_layers: vec![test_digest("orphan")],
            }),
        )])
        .await
        .unwrap();
        let mut builder = sm.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.unwrap();
        let mut restored = CouncilStateMachine::new();
        restored
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        assert_eq!(
            restored.desired_state().await.registry_gc_generations[&1],
            1
        );
        let mut commit = test_manifest_commit();
        commit.holder_nodes = std::collections::BTreeSet::from([1]);
        let refused = restored
            .apply_fixture(vec![normal_entry(
                1,
                2,
                RaftRequest::ManifestCommit(commit.clone()),
            )])
            .await
            .unwrap();
        assert!(matches!(
            refused[0],
            CouncilResponse::RegistryPublicationStale
        ));
        commit.observed_gc_generation = 1;
        let digest = commit.manifest.digest.clone();
        let accepted = restored
            .apply_fixture(vec![normal_entry(
                1,
                3,
                RaftRequest::ManifestCommit(commit),
            )])
            .await
            .unwrap();
        assert!(matches!(accepted[0], CouncilResponse::Applied { .. }));
        let response = restored
            .apply_fixture(vec![normal_entry(
                1,
                4,
                RaftRequest::GcReport(crate::pickle::types::GcReport {
                    node_id: 1,
                    deleted_layers: vec![digest],
                }),
            )])
            .await
            .unwrap();
        assert!(
            matches!(&response[0], CouncilResponse::GcApproved { approved } if approved.is_empty())
        );
        assert_eq!(
            restored.desired_state().await.registry_gc_generations[&1],
            1
        );
    }

    #[tokio::test]
    async fn apply_manifest_commit_updates_catalog() {
        let mut sm = CouncilStateMachine::new();
        let commit = test_manifest_commit();
        let entry = normal_entry(1, 1, RaftRequest::ManifestCommit(commit));

        sm.apply_fixture(vec![entry]).await.unwrap();

        let state = sm.desired_state().await;
        let found = state
            .manifest_catalog
            .get_manifest_by_tag("myapp", "latest");
        assert!(found.is_some());
        assert_eq!(found.unwrap().repository, "myapp");
    }

    #[tokio::test]
    async fn repository_ownership_survives_snapshot_and_independent_tag_retirement() {
        let mut state_machine = CouncilStateMachine::new();
        let original = test_manifest_commit();
        let mut copy = original.clone();
        copy.manifest.repository = "team-copy/app".into();
        state_machine
            .apply_fixture(vec![
                normal_entry(1, 1, RaftRequest::ManifestCommit(original)),
                normal_entry(1, 2, RaftRequest::ManifestCommit(copy)),
            ])
            .await
            .unwrap();
        assert_eq!(
            state_machine
                .desired_state()
                .await
                .manifest_catalog
                .manifests
                .len(),
            2
        );
        let mut builder = state_machine.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.unwrap();
        let mut restored = CouncilStateMachine::new();
        restored
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        restored
            .apply_fixture(vec![normal_entry(
                1,
                3,
                RaftRequest::DeleteTag(crate::pickle::types::DeleteTag {
                    repository: "team-copy/app".into(),
                    tag: "latest".into(),
                }),
            )])
            .await
            .unwrap();
        let state = restored.desired_state().await;
        assert_eq!(state.manifest_catalog.manifests.len(), 1);
        let retained = state
            .manifest_catalog
            .get_manifest_by_tag("myapp", "latest")
            .unwrap();
        assert_eq!(retained.repository, "myapp");
        assert_eq!(
            retained.tags,
            std::collections::BTreeSet::from(["latest".into()])
        );
    }

    #[tokio::test]
    async fn apply_gc_report_removes_holder() {
        let mut sm = CouncilStateMachine::new();

        // First: set up layer locations
        let digest = test_digest("layer1");
        let update = crate::pickle::types::UpdateLayerLocations {
            updates: vec![(digest.clone(), std::collections::BTreeSet::from([1, 2, 3]))],
        };
        sm.inner
            .write()
            .await
            .state
            .manifest_catalog
            .apply_update_locations(&update);

        // Then: GC report removes node 2
        let report = crate::pickle::types::GcReport {
            node_id: 2,
            deleted_layers: vec![digest.clone()],
        };
        let responses = sm
            .apply_fixture(vec![normal_entry(1, 2, RaftRequest::GcReport(report))])
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
        sm.inner
            .write()
            .await
            .state
            .manifest_catalog
            .apply_update_locations(&update);

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
            .apply_fixture(vec![
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
        sm.apply_fixture(vec![normal_entry(
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
        sm.apply_fixture(vec![normal_entry(1, 2, RaftRequest::DeleteTag(delete))])
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

    /// #477: a recovered council already holds the restored security state
    /// (CAs, API and join tokens, the CRL). The node's init-time bootstrap
    /// file has CAs but no tokens; committing it on top wiped every token.
    #[test]
    fn security_state_init_never_replaces_an_initialised_security_state() {
        let mut inner = StateMachineInner::default();
        let hierarchy = crate::sesame::ca::generate_ca_hierarchy("restored", &[3; 32]).unwrap();
        inner.state.security_state = crate::sesame::types::SecurityState {
            certificate_authorities: vec![hierarchy.root.ca.clone()],
            api_tokens: vec![crate::sesame::types::ApiToken {
                name: "admin".to_string(),
                token_hash: vec![1],
                token_salt: vec![2],
                role: crate::sesame::types::ApiRole::Admin,
                scope: crate::sesame::types::TokenScope::default(),
                expires_at: None,
                created_at: std::time::SystemTime::UNIX_EPOCH,
            }],
            next_serial: 900,
            ..Default::default()
        };
        let bootstrap = crate::sesame::types::SecurityState {
            certificate_authorities: vec![hierarchy.root.ca],
            next_serial: 2,
            ..Default::default()
        };
        let response = inner.apply_request(&RaftRequest::SecurityStateInit(Box::new(bootstrap)));
        assert!(
            matches!(response, Some(CouncilResponse::Refused { .. })),
            "a second security bootstrap must be refused, got {response:?}"
        );
        assert_eq!(inner.state.security_state.api_tokens.len(), 1);
        assert_eq!(inner.state.security_state.next_serial, 900);
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
        // Two rotations: gens 0 and 1 are read-only, gen 2 is active.
        for (generation, read_only) in [(0, true), (1, true), (2, false)] {
            inner
                .state
                .security_state
                .age_keypairs
                .push(test_age_keypair(
                    AgeKeyScope::ClusterWide,
                    generation,
                    read_only,
                ));
        }

        inner.apply_request(&RaftRequest::FinalizeSecretRotation {
            scope: AgeKeyScope::ClusterWide,
        });

        let remaining: Vec<(u64, bool)> = inner
            .state
            .security_state
            .age_keypairs
            .iter()
            .map(|kp| (kp.generation, kp.read_only))
            .collect();
        // Gen 1 is retired; gen 0 stays, read-only, because it opens the
        // root CA backup `relish init` wrote (F04 R0).
        assert_eq!(remaining, [(0, true), (2, false)]);
    }

    /// F04 R1 through the log: a Node CA rotation begins, a second one is
    /// refused, finalise is refused while a node still holds a leaf from the
    /// retiring CA, and goes through once that node's renewal is allocated.
    #[test]
    fn ca_rotation_through_the_log_waits_for_every_node_leaf() {
        use crate::sesame::types::{CaRole, CertificateAuthority, SerialNumber};
        let mut inner = StateMachineInner::default();
        let hierarchy = crate::sesame::ca::generate_ca_hierarchy("rotate", &[4; 32]).unwrap();
        inner.state.security_state = crate::sesame::types::SecurityState {
            certificate_authorities: vec![
                CertificateAuthority {
                    private_key_wrapped: None,
                    ..hierarchy.root.ca.clone()
                },
                hierarchy.node.ca.clone(),
            ],
            next_serial: 10,
            ..Default::default()
        };
        // node-a joins under generation 0.
        inner
            .state
            .security_state
            .join_tokens
            .push(crate::sesame::types::JoinToken {
                token_hash: [9; 32],
                node_id: "node-a".into(),
                expires_at: std::time::SystemTime::now() + std::time::Duration::from_secs(60),
                consumed: false,
                attestation_mode: crate::sesame::types::AttestationMode::None,
            });
        inner.apply_request(&RaftRequest::ConsumeJoinTokenForIssue {
            token_hash: [9; 32],
        });
        assert_eq!(
            inner.state.security_state.node_leaves["node-a"].ca_generation,
            0
        );

        let successor = |generation| {
            let generated = crate::sesame::ca::generate_intermediate_ca(
                CaRole::Node,
                "rotate",
                SerialNumber(50 + generation),
                hierarchy.root.ca.serial,
                &hierarchy.root.signing_keypair,
                &hierarchy.root.certificate_params,
                &[4; 32],
            )
            .unwrap();
            Box::new(CertificateAuthority {
                generation,
                ..generated.ca
            })
        };
        let new_ca = successor(1);
        let begin = inner.apply_request(&RaftRequest::CaRotationBegin {
            role: CaRole::Node,
            ca: new_ca.clone(),
        });
        assert_eq!(begin, None);
        assert_eq!(
            inner
                .state
                .security_state
                .active_ca(CaRole::Node)
                .unwrap()
                .generation,
            1
        );
        let stacked = inner.apply_request(&RaftRequest::CaRotationBegin {
            role: CaRole::Node,
            ca: successor(2),
        });
        assert!(matches!(stacked, Some(CouncilResponse::Refused { .. })));

        let now_unix_ms = new_ca
            .not_before
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 60_000;
        let early = inner.apply_request(&RaftRequest::CaRotationFinalize {
            role: CaRole::Node,
            now_unix_ms,
        });
        let Some(CouncilResponse::Refused { reason }) = early else {
            panic!("finalise must be refused while node-a holds an old leaf: {early:?}");
        };
        assert!(reason.contains("node-a"), "{reason}");

        // F04 R4: node-a says it trusts the new CA, then renews onto it.
        assert_eq!(
            inner.apply_request(&RaftRequest::AcknowledgeNodeTrust {
                node_id: "node-a".into(),
                generation: 1,
            }),
            None
        );
        let still_old_leaf = inner.apply_request(&RaftRequest::CaRotationFinalize {
            role: CaRole::Node,
            now_unix_ms,
        });
        assert!(
            matches!(&still_old_leaf, Some(CouncilResponse::Refused { reason }) if reason.contains("leaves from the retiring CA")),
            "{still_old_leaf:?}"
        );
        inner.apply_request(&RaftRequest::AllocateNodeSerial {
            node_id: "node-a".into(),
        });
        let done = inner.apply_request(&RaftRequest::CaRotationFinalize {
            role: CaRole::Node,
            now_unix_ms,
        });
        assert_eq!(done, None);
        assert_eq!(
            inner.state.security_state.trusted_cas(CaRole::Node).len(),
            1
        );
    }

    /// F04 R4 through the log: prepare allocates the serial the operator's
    /// certificate must carry and answers with it, a prepare during a
    /// rotation is refused, and so is an acknowledgement from a node the
    /// council has no leaf for.
    #[test]
    fn ca_rotation_prepare_and_trust_acknowledgements_through_the_log() {
        use crate::sesame::types::{CaRole, CertificateAuthority};
        let mut inner = StateMachineInner::default();
        let hierarchy = crate::sesame::ca::generate_ca_hierarchy("rotate", &[4; 32]).unwrap();
        inner.state.security_state = crate::sesame::types::SecurityState {
            certificate_authorities: vec![
                CertificateAuthority {
                    private_key_wrapped: None,
                    ..hierarchy.root.ca.clone()
                },
                hierarchy.node.ca.clone(),
            ],
            next_serial: 10,
            ..Default::default()
        };
        let (csr_der, private_key_wrapped) =
            crate::sesame::ca::create_intermediate_csr(CaRole::Node, &[4; 32]).unwrap();
        let prepare = RaftRequest::CaRotationPrepare {
            role: CaRole::Node,
            generation: 1,
            csr_der,
            private_key_wrapped,
        };
        assert_eq!(
            inner.apply_request(&prepare),
            Some(CouncilResponse::SerialAllocated { serial: 10 })
        );
        assert_eq!(inner.state.security_state.pending_intermediates.len(), 1);

        let stranger = inner.apply_request(&RaftRequest::AcknowledgeNodeTrust {
            node_id: "stranger".into(),
            generation: 0,
        });
        assert!(matches!(stranger, Some(CouncilResponse::Refused { .. })));

        let generated = crate::sesame::ca::generate_intermediate_ca(
            CaRole::Node,
            "rotate",
            crate::sesame::types::SerialNumber(50),
            hierarchy.root.ca.serial,
            &hierarchy.root.signing_keypair,
            &hierarchy.root.certificate_params,
            &[4; 32],
        )
        .unwrap();
        inner.apply_request(&RaftRequest::CaRotationBegin {
            role: CaRole::Node,
            ca: Box::new(CertificateAuthority {
                generation: 1,
                ..generated.ca
            }),
        });
        assert!(inner.state.security_state.pending_intermediates.is_empty());
        assert!(matches!(
            inner.apply_request(&prepare),
            Some(CouncilResponse::Refused { .. })
        ));
    }

    /// Only the cluster-wide generation-0 key opens the root CA backup; a
    /// namespace's old keys are retired as before.
    #[test]
    fn finalize_retires_a_namespaces_generation_zero() {
        use crate::sesame::types::AgeKeyScope;
        let scope = AgeKeyScope::Namespace("team-a".into());
        let mut inner = StateMachineInner::default();
        for (generation, read_only) in [(0, true), (1, false)] {
            inner
                .state
                .security_state
                .age_keypairs
                .push(test_age_keypair(scope.clone(), generation, read_only));
        }
        inner.apply_request(&RaftRequest::FinalizeSecretRotation {
            scope: scope.clone(),
        });
        let remaining: Vec<u64> = inner
            .state
            .security_state
            .age_keypairs
            .iter()
            .map(|kp| kp.generation)
            .collect();
        assert_eq!(remaining, [1]);
    }

    fn app_with_secret(value: &str) -> crate::config::app::AppSpec {
        toml::from_str(&format!(
            "image = \"web:v1\"\n[env]\nDB_PASSWORD = \"{value}\"\nMODE = \"plain\"\n"
        ))
        .unwrap()
    }

    fn reseal(app: &AppId, previous: &str, sealed: &str) -> crate::sesame::types::ResealedSecret {
        crate::sesame::types::ResealedSecret {
            app_id: app.clone(),
            env_key: "DB_PASSWORD".to_string(),
            previous: previous.to_string(),
            sealed: sealed.to_string(),
        }
    }

    /// A state with the cluster key at generation 0 and one app with a
    /// cluster-sealed value in each of team-a and team-b.
    fn two_tenant_state() -> (StateMachineInner, AppId, AppId) {
        use crate::sesame::types::AgeKeyScope;
        let mut inner = StateMachineInner::default();
        inner
            .state
            .security_state
            .age_keypairs
            .push(test_age_keypair(AgeKeyScope::ClusterWide, 0, false));
        let web = AppId::new("web", "team-a");
        let api = AppId::new("api", "team-b");
        for (app, value) in [(&web, "ENC[AGE:a-old]"), (&api, "ENC[AGE:b-old]")] {
            inner.apply_request(&RaftRequest::AppSpec {
                app_id: app.clone(),
                spec: Box::new(app_with_secret(value)),
            });
        }
        (inner, web, api)
    }

    #[test]
    fn a_namespaces_first_key_reseals_its_values_in_the_same_entry() {
        use crate::sesame::types::AgeKeyScope;
        let (mut inner, web, api) = two_tenant_state();
        let team_a = AgeKeyScope::Namespace("team-a".into());

        let response = inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: team_a.clone(),
            new_keypair: test_age_keypair(team_a.clone(), 0, false),
            resealed: vec![reseal(&web, "ENC[AGE:a-old]", "ENC[AGE:a-new]")],
        });

        assert!(
            !matches!(response, Some(CouncilResponse::Refused { .. })),
            "{response:?}"
        );
        assert_eq!(
            inner.state.apps[&web].env["DB_PASSWORD"].as_str(),
            "ENC[AGE:a-new]"
        );
        assert_eq!(inner.state.apps[&web].env["MODE"].as_str(), "plain");
        assert_eq!(
            inner.state.apps[&api].env["DB_PASSWORD"].as_str(),
            "ENC[AGE:b-old]",
            "another namespace's values are untouched"
        );
        let seal = &inner.state.security_state.secret_seals["team-a/web/DB_PASSWORD"];
        assert_eq!((&seal.scope, seal.generation), (&team_a, 0));
        assert!(
            inner
                .state
                .security_state
                .age_keypairs
                .iter()
                .all(|kp| !kp.read_only),
            "a namespace's first key starts no cluster rotation"
        );
    }

    #[test]
    fn a_stale_reseal_is_refused_and_creates_no_key() {
        use crate::sesame::types::AgeKeyScope;
        let (mut inner, web, _) = two_tenant_state();
        // An apply landed after the leader read the value.
        inner.apply_request(&RaftRequest::AppSpec {
            app_id: web.clone(),
            spec: Box::new(app_with_secret("ENC[AGE:a-reapplied]")),
        });
        let team_a = AgeKeyScope::Namespace("team-a".into());

        let response = inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: team_a.clone(),
            new_keypair: test_age_keypair(team_a, 0, false),
            resealed: vec![reseal(&web, "ENC[AGE:a-old]", "ENC[AGE:a-new]")],
        });

        assert!(
            matches!(response, Some(CouncilResponse::Refused { ref reason }) if reason.contains("stale")),
            "{response:?}"
        );
        assert!(!inner.state.security_state.has_namespace_key("team-a"));
        assert_eq!(
            inner.state.apps[&web].env["DB_PASSWORD"].as_str(),
            "ENC[AGE:a-reapplied]"
        );
    }

    #[test]
    fn a_namespace_key_cannot_reseal_another_namespaces_values() {
        use crate::sesame::types::AgeKeyScope;
        let (mut inner, _, api) = two_tenant_state();
        let team_a = AgeKeyScope::Namespace("team-a".into());

        let response = inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: team_a.clone(),
            new_keypair: test_age_keypair(team_a, 0, false),
            resealed: vec![reseal(&api, "ENC[AGE:b-old]", "ENC[AGE:stolen]")],
        });

        assert!(
            matches!(response, Some(CouncilResponse::Refused { .. })),
            "{response:?}"
        );
        assert_eq!(
            inner.state.apps[&api].env["DB_PASSWORD"].as_str(),
            "ENC[AGE:b-old]"
        );
        assert!(!inner.state.security_state.has_namespace_key("team-a"));
    }

    #[test]
    fn only_a_namespaces_first_key_reseals() {
        use crate::sesame::types::AgeKeyScope;
        let (mut inner, web, _) = two_tenant_state();
        let team_a = AgeKeyScope::Namespace("team-a".into());
        inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: team_a.clone(),
            new_keypair: test_age_keypair(team_a.clone(), 0, false),
            resealed: Vec::new(),
        });

        for scope in [team_a.clone(), AgeKeyScope::ClusterWide] {
            let response = inner.apply_request(&RaftRequest::RotateSecretKey {
                scope: scope.clone(),
                new_keypair: test_age_keypair(scope.clone(), 1, false),
                resealed: vec![reseal(&web, "ENC[AGE:a-old]", "ENC[AGE:a-new]")],
            });
            assert!(
                matches!(response, Some(CouncilResponse::Refused { .. })),
                "{scope:?}: {response:?}"
            );
        }
        assert_eq!(
            inner.state.apps[&web].env["DB_PASSWORD"].as_str(),
            "ENC[AGE:a-old]"
        );
    }

    /// A namespace's rotation and finalise touch only its own keys: the
    /// cluster scope neither rotates with it nor waits for its secrets.
    #[test]
    fn namespace_rotation_and_finalise_leave_the_cluster_scope_alone() {
        use crate::sesame::types::AgeKeyScope;
        let (mut inner, web, _) = two_tenant_state();
        let team_a = AgeKeyScope::Namespace("team-a".into());
        inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: team_a.clone(),
            new_keypair: test_age_keypair(team_a.clone(), 0, false),
            resealed: vec![reseal(&web, "ENC[AGE:a-old]", "ENC[AGE:a-gen0]")],
        });
        inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: team_a.clone(),
            new_keypair: test_age_keypair(team_a.clone(), 1, false),
            resealed: Vec::new(),
        });
        let cluster_read_only = |inner: &StateMachineInner| {
            inner
                .state
                .security_state
                .age_keypairs
                .iter()
                .filter(|kp| kp.scope == AgeKeyScope::ClusterWide)
                .any(|kp| kp.read_only)
        };
        assert!(!cluster_read_only(&inner));

        // team-a's value is still under generation 0: finalise waits.
        let response = inner.apply_request(&RaftRequest::FinalizeSecretRotation {
            scope: team_a.clone(),
        });
        assert!(
            matches!(response, Some(CouncilResponse::Refused { ref reason })
                if reason.contains("team-a/web/DB_PASSWORD") && !reason.contains("team-b")),
            "{response:?}"
        );

        // Re-applied under generation 1, it no longer blocks.
        inner.apply_request(&RaftRequest::AppSpec {
            app_id: web.clone(),
            spec: Box::new(app_with_secret("ENC[AGE:a-gen1]")),
        });
        let response = inner.apply_request(&RaftRequest::FinalizeSecretRotation {
            scope: team_a.clone(),
        });
        assert!(
            !matches!(response, Some(CouncilResponse::Refused { .. })),
            "{response:?}"
        );
        let left: Vec<(AgeKeyScope, u64)> = inner
            .state
            .security_state
            .age_keypairs
            .iter()
            .map(|kp| (kp.scope.clone(), kp.generation))
            .collect();
        assert_eq!(left, [(AgeKeyScope::ClusterWide, 0), (team_a, 1)]);

        // A cluster rotation waits only for the cluster-sealed team-b value.
        inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: AgeKeyScope::ClusterWide,
            new_keypair: test_age_keypair(AgeKeyScope::ClusterWide, 1, false),
            resealed: Vec::new(),
        });
        let response = inner.apply_request(&RaftRequest::FinalizeSecretRotation {
            scope: AgeKeyScope::ClusterWide,
        });
        assert!(
            matches!(response, Some(CouncilResponse::Refused { ref reason })
                if reason.contains("team-b/api/DB_PASSWORD") && !reason.contains("team-a")),
            "{response:?}"
        );
    }

    /// F04 R0: `relish init` seals the root CA's private key to the cluster's
    /// generation-0 age key, in `<cluster>-root-ca.age`. Finalising a secret
    /// rotation used to drop that key, after which nothing the cluster holds
    /// could open the root backup.
    #[test]
    fn the_root_backup_still_opens_after_a_finalised_secret_rotation() {
        use crate::sesame::types::AgeKeyScope;
        let dir = tempfile::tempdir().unwrap();
        let init = crate::sesame::init::initialize_cluster("prod", "node-1", dir.path()).unwrap();
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::SecurityStateInit(Box::new(
            init.security_state.clone(),
        )));
        let (next, _) = crate::sesame::secret::generate_age_keypair(
            AgeKeyScope::ClusterWide,
            &init.master_secret,
            1,
        )
        .unwrap();
        inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: AgeKeyScope::ClusterWide,
            new_keypair: next,
            resealed: Vec::new(),
        });
        let response = inner.apply_request(&RaftRequest::FinalizeSecretRotation {
            scope: AgeKeyScope::ClusterWide,
        });
        assert!(
            !matches!(response, Some(CouncilResponse::Refused { .. })),
            "{response:?}"
        );

        let sealed = std::fs::read(&init.sealed_root_ca_path).unwrap();
        let opened = inner
            .state
            .security_state
            .age_keypairs
            .iter()
            .filter(|kp| kp.scope == AgeKeyScope::ClusterWide)
            .filter_map(|kp| {
                crate::sesame::secret::unwrap_age_identity(kp, &init.master_secret).ok()
            })
            .find_map(|identity| crate::sesame::secret::unseal_with_age(&sealed, &identity).ok());
        assert!(
            opened.is_some(),
            "no key left in the state opens the root CA backup"
        );
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

        let response = inner.apply_request(&RaftRequest::FinalizeSecretRotation {
            scope: AgeKeyScope::ClusterWide,
        });

        assert_eq!(
            inner.state.security_state.age_keypairs.len(),
            1,
            "no active key means nothing is retired"
        );
        assert!(
            matches!(response, Some(CouncilResponse::Refused { .. })),
            "the stray finalize is refused, not silently ignored"
        );
    }

    /// App spec with one encrypted and one plain env value.
    fn spec_with_encrypted_env() -> crate::config::app::AppSpec {
        toml::from_str(
            r#"
            image = "t:v1"
            [env]
            DB_PASSWORD = "ENC[AGE:c2VhbGVk]"
            LOG_LEVEL = "info"
            "#,
        )
        .unwrap()
    }

    fn apply_app(inner: &mut StateMachineInner, name: &str, namespace: &str) {
        inner.apply_request(&RaftRequest::AppSpec {
            app_id: crate::meat::types::AppId::new(name, namespace),
            spec: Box::new(spec_with_encrypted_env()),
        });
    }

    /// PKI8: finalising while a stored secret is still sealed under the
    /// old generation is refused, and the refusal names the secret.
    #[test]
    fn finalize_refused_while_a_secret_is_sealed_under_an_old_generation() {
        use crate::sesame::types::AgeKeyScope;
        let mut inner = StateMachineInner::default();
        inner
            .state
            .security_state
            .age_keypairs
            .push(test_age_keypair(AgeKeyScope::ClusterWide, 0, false));
        // The app (and its seal record at generation 0) lands first…
        apply_app(&mut inner, "web", "default");
        // …then a rotation adds generation 1 and retires generation 0.
        inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: AgeKeyScope::ClusterWide,
            new_keypair: test_age_keypair(AgeKeyScope::ClusterWide, 1, false),
            resealed: Vec::new(),
        });

        let response = inner.apply_request(&RaftRequest::FinalizeSecretRotation {
            scope: AgeKeyScope::ClusterWide,
        });

        let Some(CouncilResponse::Refused { reason }) = response else {
            panic!("expected a refusal, got {response:?}");
        };
        assert!(
            reason.contains("default/web/DB_PASSWORD"),
            "the refusal names the stale secret: {reason}"
        );
        assert!(
            !reason.contains("LOG_LEVEL"),
            "plain values are not implicated: {reason}"
        );
        assert_eq!(
            inner.state.security_state.age_keypairs.len(),
            2,
            "the old key survives — the secret is still decryptable"
        );
    }

    /// PKI8: after the operator re-encrypts (re-applies the spec under the
    /// new generation), finalize retires the old key as before.
    #[test]
    fn finalize_succeeds_after_secrets_re_encrypted_under_the_new_generation() {
        use crate::sesame::types::AgeKeyScope;
        let mut inner = StateMachineInner::default();
        inner
            .state
            .security_state
            .age_keypairs
            .push(test_age_keypair(AgeKeyScope::ClusterWide, 0, false));
        apply_app(&mut inner, "web", "default");
        inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: AgeKeyScope::ClusterWide,
            new_keypair: test_age_keypair(AgeKeyScope::ClusterWide, 1, false),
            resealed: Vec::new(),
        });

        // The re-encrypt step: the spec is re-applied, which re-records
        // its seals against the now-active generation 1.
        apply_app(&mut inner, "web", "default");

        let response = inner.apply_request(&RaftRequest::FinalizeSecretRotation {
            scope: AgeKeyScope::ClusterWide,
        });

        assert!(
            !matches!(response, Some(CouncilResponse::Refused { .. })),
            "finalize proceeds once everything is re-sealed: {response:?}"
        );
        let remaining: Vec<(u64, bool)> = inner
            .state
            .security_state
            .age_keypairs
            .iter()
            .map(|kp| (kp.generation, kp.read_only))
            .collect();
        // Gen 1 is the only active key; gen 0 stays read-only for the root
        // CA backup (F04 R0).
        assert_eq!(remaining, [(0, true), (1, false)]);
    }

    /// PKI8: a second rotation while one is un-finalised is refused; the
    /// key set is unchanged.
    #[test]
    fn concurrent_second_rotation_refused_until_finalised() {
        use crate::sesame::types::AgeKeyScope;
        let mut inner = StateMachineInner::default();
        inner
            .state
            .security_state
            .age_keypairs
            .push(test_age_keypair(AgeKeyScope::ClusterWide, 0, false));
        inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: AgeKeyScope::ClusterWide,
            new_keypair: test_age_keypair(AgeKeyScope::ClusterWide, 1, false),
            resealed: Vec::new(),
        });

        let response = inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: AgeKeyScope::ClusterWide,
            new_keypair: test_age_keypair(AgeKeyScope::ClusterWide, 2, false),
            resealed: Vec::new(),
        });

        let Some(CouncilResponse::Refused { reason }) = response else {
            panic!("expected a refusal, got {response:?}");
        };
        assert!(reason.contains("finalise"), "tells the operator what to do");
        let generations: Vec<u64> = inner
            .state
            .security_state
            .age_keypairs
            .iter()
            .map(|kp| kp.generation)
            .collect();
        assert_eq!(generations, vec![0, 1], "generation 2 was not added");
    }

    /// PKI8: a duplicate delivery of the *same* rotation (same new
    /// generation) is accepted idempotently — no refusal, no second key.
    #[test]
    fn same_generation_rotation_retry_is_idempotent() {
        use crate::sesame::types::AgeKeyScope;
        let mut inner = StateMachineInner::default();
        inner
            .state
            .security_state
            .age_keypairs
            .push(test_age_keypair(AgeKeyScope::ClusterWide, 0, false));
        inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: AgeKeyScope::ClusterWide,
            new_keypair: test_age_keypair(AgeKeyScope::ClusterWide, 1, false),
            resealed: Vec::new(),
        });

        let response = inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: AgeKeyScope::ClusterWide,
            new_keypair: test_age_keypair(AgeKeyScope::ClusterWide, 1, false),
            resealed: Vec::new(),
        });

        assert!(
            !matches!(response, Some(CouncilResponse::Refused { .. })),
            "a retry of the same rotation must not be refused"
        );
        assert_eq!(
            inner.state.security_state.age_keypairs.len(),
            2,
            "no duplicate keypair for generation 1"
        );
    }

    /// PKI8: an encrypted secret with no recorded seal has an unknown
    /// generation, so it blocks finalize until re-encrypted.
    #[test]
    fn secret_without_a_recorded_seal_blocks_finalize() {
        use crate::sesame::types::AgeKeyScope;

        let mut inner = StateMachineInner::default();
        inner
            .state
            .security_state
            .age_keypairs
            .push(test_age_keypair(AgeKeyScope::ClusterWide, 0, false));
        // Insert the app directly so no seal entry is recorded.
        inner.state.apps.insert(
            crate::meat::types::AppId::new("web", "default"),
            spec_with_encrypted_env(),
        );

        inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: AgeKeyScope::ClusterWide,
            new_keypair: test_age_keypair(AgeKeyScope::ClusterWide, 1, false),
            resealed: Vec::new(),
        });
        let response = inner.apply_request(&RaftRequest::FinalizeSecretRotation {
            scope: AgeKeyScope::ClusterWide,
        });

        let Some(CouncilResponse::Refused { reason }) = response else {
            panic!("expected a refusal, got {response:?}");
        };
        assert!(reason.contains("default/web/DB_PASSWORD"), "{reason}");

        // Re-encrypting (re-applying the spec) records the seal and
        // unblocks the finalize.
        apply_app(&mut inner, "web", "default");
        let response = inner.apply_request(&RaftRequest::FinalizeSecretRotation {
            scope: AgeKeyScope::ClusterWide,
        });
        assert!(!matches!(response, Some(CouncilResponse::Refused { .. })));
    }

    /// Seals follow the app: deleting it clears them, so a deleted app
    /// can never block a rotation from finalising.
    #[test]
    fn app_delete_clears_its_secret_seals() {
        use crate::sesame::types::AgeKeyScope;
        let mut inner = StateMachineInner::default();
        inner
            .state
            .security_state
            .age_keypairs
            .push(test_age_keypair(AgeKeyScope::ClusterWide, 0, false));
        apply_app(&mut inner, "web", "default");
        assert_eq!(inner.state.security_state.secret_seals.len(), 1);

        inner.apply_request(&RaftRequest::AppDelete {
            app_id: crate::meat::types::AppId::new("web", "default"),
        });
        assert!(inner.state.security_state.secret_seals.is_empty());
    }

    fn cpu_block(namespace: &str) -> crate::meat::quota::QuotaError {
        crate::meat::quota::QuotaError::CpuExceeded {
            namespace: namespace.to_string(),
            current: 0,
            requested: 1600,
            limit: 1000,
        }
    }

    /// The leader replaces the whole set: an app that fits again drops out,
    /// and a reason for an app no longer in desired state is never kept.
    #[test]
    fn quota_blocked_replaces_the_set_and_ignores_unknown_apps() {
        let mut inner = StateMachineInner::default();
        apply_app(&mut inner, "web", "prod");
        apply_app(&mut inner, "api", "prod");
        let web = crate::meat::types::AppId::new("web", "prod");
        let api = crate::meat::types::AppId::new("api", "prod");
        let ghost = crate::meat::types::AppId::new("ghost", "prod");

        inner.apply_request(&RaftRequest::QuotaBlocked {
            blocked: vec![
                (web.clone(), cpu_block("prod")),
                (ghost.clone(), cpu_block("prod")),
            ],
        });
        assert_eq!(
            inner.state.quota_blocked.get(&web),
            Some(&cpu_block("prod"))
        );
        assert!(!inner.state.quota_blocked.contains_key(&ghost));

        inner.apply_request(&RaftRequest::QuotaBlocked {
            blocked: vec![(api.clone(), cpu_block("prod"))],
        });
        assert!(!inner.state.quota_blocked.contains_key(&web));
        assert!(inner.state.quota_blocked.contains_key(&api));
    }

    /// Deleting a blocked app takes its reason with it.
    #[test]
    fn app_delete_clears_its_quota_block() {
        let mut inner = StateMachineInner::default();
        apply_app(&mut inner, "web", "prod");
        let web = crate::meat::types::AppId::new("web", "prod");
        inner.apply_request(&RaftRequest::QuotaBlocked {
            blocked: vec![(web.clone(), cpu_block("prod"))],
        });

        inner.apply_request(&RaftRequest::AppDelete { app_id: web });
        assert!(inner.state.quota_blocked.is_empty());
    }

    /// The reason survives a snapshot, so a follower that catches up from
    /// one answers the same as the leader.
    #[test]
    fn quota_blocked_survives_a_snapshot_round_trip() {
        let mut inner = StateMachineInner::default();
        apply_app(&mut inner, "web", "prod");
        let web = crate::meat::types::AppId::new("web", "prod");
        inner.apply_request(&RaftRequest::QuotaBlocked {
            blocked: vec![(web.clone(), cpu_block("prod"))],
        });
        let json = serde_json::to_vec(&inner.state).unwrap();
        let restored: DesiredState = serde_json::from_slice(&json).unwrap();
        assert_eq!(restored.quota_blocked.get(&web), Some(&cpu_block("prod")));
    }

    #[test]
    fn apply_create_join_token() {
        let mut inner = StateMachineInner::default();
        let jt = crate::sesame::types::JoinToken {
            token_hash: [0xAB; 32],
            expires_at: std::time::SystemTime::now(),
            consumed: false,
            attestation_mode: crate::sesame::types::AttestationMode::None,
            node_id: "node-02".to_string(),
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
            node_id: "node-02".to_string(),
        };
        inner.apply_request(&RaftRequest::CreateJoinToken(jt));
        inner.apply_request(&RaftRequest::ConsumeJoinToken {
            token_hash: [0xAB; 32],
        });
        assert!(inner.state.security_state.join_tokens[0].consumed);
    }

    /// O5: creating a new join token prunes previously-consumed ones so the
    /// list can't grow without bound.
    #[test]
    fn creating_a_join_token_prunes_consumed_ones() {
        let mut inner = StateMachineInner::default();
        let token = |b: u8| crate::sesame::types::JoinToken {
            token_hash: [b; 32],
            expires_at: std::time::SystemTime::now(),
            consumed: false,
            attestation_mode: crate::sesame::types::AttestationMode::None,
            node_id: "node-02".to_string(),
        };
        inner.apply_request(&RaftRequest::CreateJoinToken(token(0xAA)));
        inner.apply_request(&RaftRequest::ConsumeJoinToken {
            token_hash: [0xAA; 32],
        });
        // A second create prunes the consumed one and keeps the new one.
        inner.apply_request(&RaftRequest::CreateJoinToken(token(0xBB)));
        let tokens = &inner.state.security_state.join_tokens;
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token_hash, [0xBB; 32]);
    }

    #[test]
    fn consume_join_token_for_issue_is_atomic_and_single_use() {
        // PKI5: one atomic entry consumes the token and allocates a serial.
        // A second application of the same entry (a racer / retry) is refused,
        // so a token can only ever mint one serial.
        let mut inner = StateMachineInner::default();
        inner.state.security_state.next_serial = 7;
        let jt = crate::sesame::types::JoinToken {
            token_hash: [0xCD; 32],
            expires_at: std::time::SystemTime::now(),
            consumed: false,
            attestation_mode: crate::sesame::types::AttestationMode::None,
            node_id: "node-02".to_string(),
        };
        inner.apply_request(&RaftRequest::CreateJoinToken(jt));

        let first = inner.apply_request(&RaftRequest::ConsumeJoinTokenForIssue {
            token_hash: [0xCD; 32],
        });
        assert_eq!(
            first,
            Some(CouncilResponse::JoinTokenConsumed { serial: 7 })
        );
        assert!(inner.state.security_state.join_tokens[0].consumed);
        assert_eq!(inner.state.security_state.next_serial, 8);

        // A second (racing) application finds it consumed → Refused, and the
        // serial counter does not advance again.
        let second = inner.apply_request(&RaftRequest::ConsumeJoinTokenForIssue {
            token_hash: [0xCD; 32],
        });
        assert!(matches!(second, Some(CouncilResponse::Refused { .. })));
        assert_eq!(inner.state.security_state.next_serial, 8);
    }

    #[test]
    fn consume_join_token_for_issue_refuses_an_unknown_token() {
        let mut inner = StateMachineInner::default();
        let resp = inner.apply_request(&RaftRequest::ConsumeJoinTokenForIssue {
            token_hash: [0x11; 32],
        });
        assert!(matches!(resp, Some(CouncilResponse::Refused { .. })));
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

    fn expiring_api_token(
        name: &str,
        role: crate::sesame::types::ApiRole,
        expires_unix_ms: Option<u64>,
    ) -> crate::sesame::types::ApiToken {
        crate::sesame::types::ApiToken {
            name: name.to_string(),
            token_hash: name.as_bytes().to_vec(),
            token_salt: vec![4, 5, 6],
            role,
            scope: crate::sesame::types::TokenScope::default(),
            expires_at: expires_unix_ms
                .map(|ms| std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms)),
            created_at: std::time::UNIX_EPOCH,
        }
    }

    const SWEEP_DAY_MS: u64 = 24 * 60 * 60 * 1000;
    const SWEEP_NOW_MS: u64 = 2_000 * SWEEP_DAY_MS;

    fn token_names(inner: &StateMachineInner) -> Vec<String> {
        inner
            .state
            .security_state
            .api_tokens
            .iter()
            .map(|token| token.name.clone())
            .collect()
    }

    #[test]
    fn sweep_removes_an_expired_token_past_the_grace_and_keeps_one_inside_it() {
        use crate::sesame::types::ApiRole;
        let mut inner = StateMachineInner::default();
        for token in [
            expiring_api_token("admin", ApiRole::Admin, None),
            expiring_api_token(
                "stale",
                ApiRole::Deployer,
                Some(SWEEP_NOW_MS - 2 * SWEEP_DAY_MS),
            ),
            expiring_api_token("recent", ApiRole::ReadOnly, Some(SWEEP_NOW_MS - 1_000)),
        ] {
            inner.apply_request(&RaftRequest::CreateApiToken(token));
        }

        let response = inner.apply_request(&RaftRequest::SweepExpiredApiTokens {
            now_unix_ms: SWEEP_NOW_MS,
        });

        assert_eq!(
            response,
            Some(CouncilResponse::ApiTokensSwept {
                removed: vec!["stale".to_string()]
            })
        );
        assert_eq!(token_names(&inner), ["admin", "recent"]);
    }

    #[test]
    fn sweep_keeps_the_last_admin_even_when_it_has_expired() {
        use crate::sesame::types::ApiRole;
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::CreateApiToken(expiring_api_token(
            "admin",
            ApiRole::Admin,
            Some(SWEEP_NOW_MS - 30 * SWEEP_DAY_MS),
        )));
        inner.apply_request(&RaftRequest::CreateApiToken(expiring_api_token(
            "ci",
            ApiRole::Deployer,
            Some(SWEEP_NOW_MS - 30 * SWEEP_DAY_MS),
        )));

        inner.apply_request(&RaftRequest::SweepExpiredApiTokens {
            now_unix_ms: SWEEP_NOW_MS,
        });

        assert_eq!(token_names(&inner), ["admin"]);
    }

    #[test]
    fn sweep_never_empties_the_token_store() {
        use crate::sesame::types::ApiRole;
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::CreateApiToken(expiring_api_token(
            "only",
            ApiRole::ReadOnly,
            Some(SWEEP_NOW_MS - 30 * SWEEP_DAY_MS),
        )));

        let response = inner.apply_request(&RaftRequest::SweepExpiredApiTokens {
            now_unix_ms: SWEEP_NOW_MS,
        });

        assert_eq!(
            response,
            Some(CouncilResponse::ApiTokensSwept { removed: vec![] })
        );
        assert_eq!(token_names(&inner), ["only"]);
    }

    #[test]
    fn sweep_is_deterministic_given_now() {
        use crate::sesame::types::ApiRole;
        let tokens = [
            expiring_api_token("a", ApiRole::Admin, Some(SWEEP_NOW_MS - 3 * SWEEP_DAY_MS)),
            expiring_api_token("b", ApiRole::Admin, Some(SWEEP_NOW_MS - 3 * SWEEP_DAY_MS)),
            expiring_api_token(
                "c",
                ApiRole::Deployer,
                Some(SWEEP_NOW_MS - 3 * SWEEP_DAY_MS),
            ),
            expiring_api_token("d", ApiRole::ReadOnly, Some(SWEEP_NOW_MS + SWEEP_DAY_MS)),
        ];
        let replica = || {
            let mut inner = StateMachineInner::default();
            for token in &tokens {
                inner.apply_request(&RaftRequest::CreateApiToken(token.clone()));
            }
            inner.apply_request(&RaftRequest::SweepExpiredApiTokens {
                now_unix_ms: SWEEP_NOW_MS,
            });
            inner
        };
        let (first, second) = (replica(), replica());
        assert_eq!(token_names(&first), token_names(&second));
        assert_eq!(token_names(&first), ["b", "d"]);

        // An earlier `now` leaves everything in place: the clock that
        // decides is the one in the entry, not the replica's.
        let mut early = StateMachineInner::default();
        for token in &tokens {
            early.apply_request(&RaftRequest::CreateApiToken(token.clone()));
        }
        early.apply_request(&RaftRequest::SweepExpiredApiTokens {
            now_unix_ms: SWEEP_NOW_MS - 3 * SWEEP_DAY_MS,
        });
        assert_eq!(token_names(&early), ["a", "b", "c", "d"]);
    }

    #[test]
    fn revoke_refuses_to_remove_the_last_admin_token() {
        use crate::sesame::types::{ApiRole, ApiToken, TokenScope};
        let api_token = |name: &str, role: ApiRole| ApiToken {
            name: name.to_string(),
            token_hash: vec![1, 2, 3],
            token_salt: vec![4, 5, 6],
            role,
            scope: TokenScope::default(),
            expires_at: None,
            created_at: std::time::SystemTime::now(),
        };

        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::CreateApiToken(api_token(
            "admin-a",
            ApiRole::Admin,
        )));
        inner.apply_request(&RaftRequest::CreateApiToken(api_token(
            "deployer",
            ApiRole::Deployer,
        )));

        // The sole Admin can't be revoked — the store must never lose its last
        // Admin at runtime (which would reopen the bootstrap allow-all).
        let resp = inner.apply_request(&RaftRequest::RevokeApiToken {
            name: "admin-a".to_string(),
        });
        assert!(matches!(resp, Some(CouncilResponse::Refused { .. })));
        assert!(
            inner
                .state
                .security_state
                .api_tokens
                .iter()
                .any(|t| t.name == "admin-a"),
            "the last Admin token must survive the refused revoke"
        );

        // A non-Admin token is always revocable, even as the only non-Admin.
        inner.apply_request(&RaftRequest::RevokeApiToken {
            name: "deployer".to_string(),
        });
        assert!(
            !inner
                .state
                .security_state
                .api_tokens
                .iter()
                .any(|t| t.name == "deployer")
        );

        // With a second Admin present, the first becomes revocable.
        inner.apply_request(&RaftRequest::CreateApiToken(api_token(
            "admin-b",
            ApiRole::Admin,
        )));
        inner.apply_request(&RaftRequest::RevokeApiToken {
            name: "admin-a".to_string(),
        });
        let admins: Vec<_> = inner
            .state
            .security_state
            .api_tokens
            .iter()
            .filter(|t| t.role == ApiRole::Admin)
            .map(|t| t.name.clone())
            .collect();
        assert_eq!(admins, vec!["admin-b".to_string()]);
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
            allow_downgrade: false,
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
    fn upgrade_update_ignores_a_concurrent_different_start() {
        // M13: a second start with a *different* upgrade id while one is active
        // must not clobber the first plan (the racy check-then-write let two
        // concurrent starts through). A same-id update still applies.
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::UpgradeUpdate {
            state: Box::new(upgrade_state("up-1")),
        });
        inner.apply_request(&RaftRequest::UpgradeUpdate {
            state: Box::new(upgrade_state("up-2")),
        });
        assert_eq!(
            inner.state.active_upgrade.as_ref().unwrap().upgrade_id,
            "up-1",
            "a concurrent different-id start must not replace the active upgrade"
        );
    }

    #[test]
    fn upgrade_update_allows_a_resume_of_a_paused_run() {
        // M13 regression guard: `upgrade resume` renames the run to a fresh id
        // and replaces the *paused* active upgrade. That must apply — the race
        // guard only blocks a different-id start against a *non-paused* active
        // upgrade, not a resume.
        let mut inner = StateMachineInner::default();
        let mut paused = upgrade_state("up-1");
        paused.phase = crate::upgrade::types::ClusterUpgradePhase::Paused {
            reason: "worker reverted".to_string(),
        };
        inner.apply_request(&RaftRequest::UpgradeUpdate {
            state: Box::new(paused),
        });
        // Resume: a fresh id, non-paused phase.
        inner.apply_request(&RaftRequest::UpgradeUpdate {
            state: Box::new(upgrade_state("up-1-resume")),
        });
        assert_eq!(
            inner.state.active_upgrade.as_ref().unwrap().upgrade_id,
            "up-1-resume",
            "a resume of a paused run must replace the active upgrade"
        );
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
    fn snapshot_with_active_upgrade_roundtrips() {
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::UpgradeUpdate {
            state: Box::new(upgrade_state("up-1")),
        });

        let json = serde_json::to_string(&inner.state).unwrap();
        let reloaded: DesiredState = serde_json::from_str(&json).unwrap();
        assert_eq!(reloaded.active_upgrade, inner.state.active_upgrade);
    }

    // -- Batch/build durable tracking (12b.2) ---------------------------------

    fn batch_record(job: &str, node: &str) -> crate::meat::batch_tracker::BatchRecord {
        crate::meat::batch_tracker::BatchRecord {
            jobs: vec![crate::meat::batch_tracker::BatchJobRecord {
                resources: crate::meat::Resources::default(),
                name: job.to_string(),
                execution_name: job.to_string(),
                spec_digest: "a".repeat(64),
                namespace: "default".to_string(),
                node: Some(crate::meat::types::NodeId::new(node)),
                status: crate::meat::batch_tracker::JobStatus::Pending,
            }],
            submitted_at_epoch_secs: 1_000_000,
        }
    }

    fn owned_batch_record(
        execution: &str,
        namespace: &str,
    ) -> crate::meat::batch_tracker::BatchRecord {
        serde_json::from_value(serde_json::json!({
            "jobs": [{
                "name": "migration",
                "execution_name": execution,
                "resources":{"cpu_millicores":0,"memory_bytes":0,"gpus":0}, "spec_digest": "a".repeat(64),
                "namespace": namespace,
                "node": "worker",
                "status": "Pending"
            }],
            "submitted_at_epoch_secs": 1_000_000
        }))
        .unwrap()
    }

    #[test]
    fn committing_an_app_first_prevents_batch_execution_ownership_from_taking_it() {
        let mut inner = StateMachineInner::default();
        let identity = AppId::new("owned-execution", "default");
        inner.apply_request(&RaftRequest::AppSpec {
            app_id: identity.clone(),
            spec: Box::new(default_spec()),
        });
        let before = inner.state.batch_state.clone();
        let response = inner.apply_request(&RaftRequest::BatchRegister {
            expected_log_id: None,
            batch: owned_batch_record(&identity.name, &identity.namespace),
        });
        assert!(
            matches!(response, Some(CouncilResponse::Refused { .. })),
            "{response:?}"
        );
        assert_eq!(inner.state.batch_state, before);
        assert_eq!(inner.state.apps[&identity], default_spec());
    }

    #[tokio::test]
    async fn batch_registration_on_the_current_revision_refuses_a_retired_target_atomically() {
        let mut sm = CouncilStateMachine::new();
        let retirement = RaftRequest::DecommissionNode {
            node_id: "retired-worker".into(),
            retired_by: "operator".into(),
            reason: "powered off".into(),
            retired_at_unix_ms: 1,
            membership_log_id: None,
        };
        let response = sm.apply([normal_entry(1, 1, retirement)]).await.unwrap();
        assert!(
            matches!(response[0], CouncilResponse::NodeDecommissioned { .. }),
            "{response:?}"
        );
        let before = sm.desired_state().await.batch_state;
        let mut batch = owned_batch_record("retired-execution", "default");
        batch.jobs[0].node = Some(NodeId::new("retired-worker"));
        let response = sm
            .apply([normal_entry(
                1,
                2,
                RaftRequest::BatchRegister {
                    expected_log_id: Some(log_id(1, 1)),
                    batch,
                },
            )])
            .await
            .unwrap();
        assert!(
            matches!(response[0], CouncilResponse::Refused { .. }),
            "{response:?}"
        );
        assert_eq!(sm.desired_state().await.batch_state, before);
    }

    #[tokio::test]
    async fn shared_admission_compares_the_previous_log_and_keeps_refused_inventory_unchanged() {
        let mut sm = CouncilStateMachine::new();
        let first = owned_batch_record("first-execution", "default");
        let second = owned_batch_record("second-execution", "default");
        let responses = sm
            .apply([
                normal_entry(
                    3,
                    10,
                    RaftRequest::BatchRegister {
                        expected_log_id: None,
                        batch: first,
                    },
                ),
                normal_entry(
                    3,
                    11,
                    RaftRequest::BatchRegister {
                        expected_log_id: None,
                        batch: second.clone(),
                    },
                ),
            ])
            .await
            .unwrap();
        assert!(matches!(
            responses[0],
            CouncilResponse::BatchRegistered { batch_id: 1 }
        ));
        assert!(matches!(responses[1], CouncilResponse::Refused { .. }));
        let state = sm.desired_state().await;
        assert_eq!(state.last_applied_log, Some(log_id(3, 11)));
        assert_eq!(state.batch_state.next_batch_id, 2);
        assert_eq!(state.batch_state.batches.len(), 1);
        assert!(
            state
                .batch_state
                .execution_owner("default", "second-execution")
                .is_none()
        );
        let response = sm
            .apply([normal_entry(
                3,
                12,
                RaftRequest::BatchRegister {
                    expected_log_id: Some(log_id(3, 11)),
                    batch: second,
                },
            )])
            .await
            .unwrap();
        assert!(matches!(
            response[0],
            CouncilResponse::BatchRegistered { batch_id: 2 }
        ));
    }

    #[tokio::test]
    async fn whole_placement_pass_has_one_revision_and_no_partial_invalid_writes() {
        let mut sm = CouncilStateMachine::new();
        let decision = |name: &str, duplicate: bool| SchedulingDecision {
            app_id: AppId::new(name, "default"),
            placements: if duplicate {
                vec![
                    Placement {
                        node_id: NodeId::new("home"),
                        resources: Resources::new(1000, 0, 0),
                        ordinal: 0
                    };
                    2
                ]
            } else {
                vec![Placement {
                    node_id: NodeId::new("home"),
                    resources: Resources::new(1000, 0, 0),
                    ordinal: 0,
                }]
            },
        };
        let response = sm
            .apply([normal_entry(
                1,
                1,
                RaftRequest::SchedulingDecisions {
                    expected_log_id: None,
                    decisions: vec![decision("first", false), decision("second", true)],
                },
            )])
            .await
            .unwrap();
        assert!(matches!(response[0], CouncilResponse::Refused { .. }));
        let state = sm.desired_state().await;
        assert!(state.scheduling.is_empty());
        assert!(state.last_placed_nodes.is_empty());
        let response = sm
            .apply([normal_entry(
                1,
                2,
                RaftRequest::SchedulingDecisions {
                    expected_log_id: Some(log_id(1, 1)),
                    decisions: vec![decision("first", false), decision("second", false)],
                },
            )])
            .await
            .unwrap();
        assert!(matches!(response[0], CouncilResponse::Applied { .. }));
        assert_eq!(sm.desired_state().await.scheduling.len(), 2);
        let before = sm.desired_state().await.scheduling;
        let response = sm
            .apply([normal_entry(
                1,
                3,
                RaftRequest::SchedulingDecisions {
                    expected_log_id: Some(log_id(1, 1)),
                    decisions: vec![decision("third", false)],
                },
            )])
            .await
            .unwrap();
        assert!(matches!(response[0], CouncilResponse::Refused { .. }));
        assert_eq!(sm.desired_state().await.scheduling, before);
    }

    #[tokio::test]
    async fn unguarded_app_placement_cannot_race_shared_capacity_admission() {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "default");
        let responses = sm
            .apply([normal_entry(
                1,
                1,
                RaftRequest::SchedulingDecision(SchedulingDecision {
                    app_id: app_id.clone(),
                    placements: vec![Placement {
                        node_id: NodeId::new("home"),
                        resources: Resources::new(8000, 0, 0),
                        ordinal: 0,
                    }],
                }),
            )])
            .await
            .unwrap();
        assert!(
            matches!(responses[0], CouncilResponse::Refused { .. }),
            "{responses:?}"
        );
        assert!(!sm.desired_state().await.scheduling.contains_key(&app_id));
    }

    #[test]
    fn committing_batch_ownership_first_atomically_refuses_app_and_leased_app_writes() {
        for leased in [false, true] {
            let mut inner = StateMachineInner::default();
            let namespace = if leased { "rbtest-run1" } else { "default" };
            if leased {
                inner.apply_request(&RaftRequest::TestLeaseCreate(test_lease("run1", 100)));
            }
            let identity = AppId::new("owned-execution", namespace);
            assert!(matches!(
                inner.apply_request(&RaftRequest::BatchRegister {
                    expected_log_id: None,
                    batch: owned_batch_record(&identity.name, namespace),
                }),
                Some(CouncilResponse::BatchRegistered { .. })
            ));
            let request = if leased {
                RaftRequest::TestLeaseAppSpec {
                    lease_id: "run1".into(),
                    observed_at_unix_ms: 20,
                    app_id: identity.clone(),
                    spec: Box::new(default_spec()),
                }
            } else {
                RaftRequest::AppSpec {
                    app_id: identity.clone(),
                    spec: Box::new(default_spec()),
                }
            };
            let response = inner.apply_request(&request);
            assert!(
                matches!(response, Some(CouncilResponse::Refused { .. })),
                "leased={leased}: {response:?}"
            );
            assert!(!inner.state.apps.contains_key(&identity));
            if leased {
                assert!(inner.state.test_leases["run1"].resources.is_empty());
            }
            // Ownership is namespaced: the same name elsewhere remains ordinary.
            assert!(
                inner
                    .apply_request(&RaftRequest::AppSpec {
                        app_id: AppId::new(&identity.name, "other"),
                        spec: Box::new(default_spec()),
                    })
                    .is_none()
            );
        }
    }

    #[test]
    fn a_batch_registration_cannot_borrow_another_batchs_execution_identity() {
        let mut inner = StateMachineInner::default();
        let request = RaftRequest::BatchRegister {
            expected_log_id: None,
            batch: owned_batch_record("owned-execution", "default"),
        };
        assert!(matches!(
            inner.apply_request(&request),
            Some(CouncilResponse::BatchRegistered { batch_id: 1 })
        ));
        let before = inner.state.batch_state.clone();
        let response = inner.apply_request(&request);
        assert!(
            matches!(response, Some(CouncilResponse::Refused { .. })),
            "{response:?}"
        );
        assert_eq!(inner.state.batch_state, before);
    }

    fn ownership_capacity_record(
        count: usize,
        long_labels: bool,
    ) -> crate::meat::batch_tracker::BatchRecord {
        let namespace = if long_labels {
            "n".repeat(63)
        } else {
            "default".to_string()
        };
        crate::meat::batch_tracker::BatchRecord {
            jobs: (0..count)
                .map(|i| {
                    let short_name = format!("execution-{i:08}");
                    let execution = if long_labels {
                        format!("{}-{i:08}", "e".repeat(54))
                    } else {
                        short_name.clone()
                    };
                    serde_json::from_value(serde_json::json!({
                        "name": if long_labels { format!("{}-{i:08}", "l".repeat(54)) } else { short_name },
                        "execution_name": execution,
                "resources":{"cpu_millicores":0,"memory_bytes":0,"gpus":0}, "spec_digest": "a".repeat(64),
                        "namespace": namespace,
                        "node": "worker",
                        "status": "Pending"
                    })).unwrap()
                })
                .collect(),
            submitted_at_epoch_secs: 1_000_000,
        }
    }

    #[test]
    fn a_full_execution_ownership_index_refuses_registration_but_keeps_active_transitions() {
        let mut inner = StateMachineInner::default();
        // The published admission contract has a 131,072-entry bound independent
        // of its encoded-byte bound. Short valid labels fit beneath both.
        let response = inner.apply_request(&RaftRequest::BatchRegister {
            expected_log_id: None,
            batch: ownership_capacity_record(131_072, false),
        });
        assert!(
            matches!(
                response,
                Some(CouncilResponse::BatchRegistered { batch_id: 1 })
            ),
            "{response:?}"
        );
        let before = inner.state.batch_state.clone();
        let response = inner.apply_request(&RaftRequest::BatchRegister {
            expected_log_id: None,
            batch: owned_batch_record("overflow-execution", "default"),
        });
        assert!(
            matches!(response, Some(CouncilResponse::Refused { .. })),
            "{response:?}"
        );
        assert_eq!(inner.state.batch_state, before);
        // Exhaustion must not prevent an already-admitted execution from
        // publishing the positive terminal result that its watcher needs.
        assert!(
            inner
                .apply_request(&RaftRequest::BatchJobUpdate {
                    batch_id: 1,
                    job_name: "execution-00000000".into(),
                    namespace: "default".into(),
                    exit_code: Some(0),
                    status: crate::meat::batch_tracker::JobStatus::Completed,
                })
                .is_none()
        );
        assert_eq!(
            inner.state.batch_state.get(1).unwrap().jobs[0].status,
            crate::meat::batch_tracker::JobStatus::Completed
        );
    }

    #[test]
    fn an_oversized_execution_ownership_index_refuses_the_whole_registration_before_mutation() {
        let mut inner = StateMachineInner::default();
        let before = inner.state.batch_state.clone();
        // Valid maximum-length labels exceed the independent 32 MiB encoded
        // bound while still obeying the entry bound. No partial ownership or
        // allocated batch ID may leak from the rejected group.
        let response = inner.apply_request(&RaftRequest::BatchRegister {
            expected_log_id: None,
            batch: ownership_capacity_record(131_072, true),
        });
        assert!(
            matches!(response, Some(CouncilResponse::Refused { .. })),
            "{response:?}"
        );
        assert_eq!(inner.state.batch_state, before);
        assert!(matches!(
            inner.apply_request(&RaftRequest::BatchRegister {
                expected_log_id: None,
                batch: owned_batch_record("small-execution", "default"),
            }),
            Some(CouncilResponse::BatchRegistered { batch_id: 1 })
        ));
    }

    fn build_record(name: &str) -> crate::bun::build_runner::BuildRecord {
        crate::bun::build_runner::BuildRecord {
            name: name.to_string(),
            runner_node: Some("n1".to_string()),
            state: crate::bun::build_runner::BuildState::Running,
            created_at_epoch_secs: 1_000_000,
        }
    }

    #[tokio::test]
    async fn batch_register_returns_the_allocated_id() {
        let mut sm = CouncilStateMachine::new();
        let responses = sm
            .apply_fixture(vec![
                normal_entry(
                    1,
                    1,
                    RaftRequest::BatchRegister {
                        expected_log_id: None,
                        batch: batch_record("j1", "n1"),
                    },
                ),
                normal_entry(
                    1,
                    2,
                    RaftRequest::BatchRegister {
                        expected_log_id: None,
                        batch: batch_record("j2", "n1"),
                    },
                ),
            ])
            .await
            .unwrap();
        assert_eq!(
            responses[0],
            CouncilResponse::BatchRegistered { batch_id: 1 }
        );
        assert_eq!(
            responses[1],
            CouncilResponse::BatchRegistered { batch_id: 2 }
        );
    }

    /// JOB4: the id counters ride the persisted snapshot, so a
    /// restarted leader continues the sequence instead of reusing ids.
    #[tokio::test]
    async fn batch_and_build_ids_survive_a_snapshot_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ids.redb");
        {
            let db = std::sync::Arc::new(Database::create(&path).unwrap());
            let mut sm = CouncilStateMachine::with_store(db).unwrap();
            sm.apply_fixture(vec![
                normal_entry(
                    1,
                    1,
                    RaftRequest::BatchRegister {
                        expected_log_id: None,
                        batch: batch_record("j1", "n1"),
                    },
                ),
                normal_entry(
                    1,
                    2,
                    RaftRequest::BuildRegister {
                        build: build_record("b1"),
                    },
                ),
            ])
            .await
            .unwrap();
            let mut builder = sm.get_snapshot_builder().await;
            builder.build_snapshot().await.unwrap();
        }

        let db = std::sync::Arc::new(Database::create(&path).unwrap());
        let mut sm = CouncilStateMachine::with_store(db).unwrap();
        let responses = sm
            .apply_fixture(vec![
                normal_entry(
                    2,
                    3,
                    RaftRequest::BatchRegister {
                        expected_log_id: None,
                        batch: batch_record("j2", "n1"),
                    },
                ),
                normal_entry(
                    2,
                    4,
                    RaftRequest::BuildRegister {
                        build: build_record("b2"),
                    },
                ),
            ])
            .await
            .unwrap();
        assert_eq!(
            responses[0],
            CouncilResponse::BatchRegistered { batch_id: 2 }
        );
        assert_eq!(
            responses[1],
            CouncilResponse::BuildRegistered { build_id: 2 }
        );

        // The pre-restart records reloaded too.
        let state = sm.desired_state().await;
        assert!(state.batch_state.get(1).is_some());
        assert!(state.build_state.get(1).is_some());
    }

    #[tokio::test]
    async fn batch_job_update_validates_transitions() {
        let mut sm = CouncilStateMachine::new();
        sm.apply_fixture(vec![normal_entry(
            1,
            1,
            RaftRequest::BatchRegister {
                expected_log_id: None,
                batch: batch_record("j1", "n1"),
            },
        )])
        .await
        .unwrap();

        // A legal completion applies…
        let ok = sm
            .apply_fixture(vec![normal_entry(
                1,
                2,
                RaftRequest::BatchJobUpdate {
                    batch_id: 1,
                    job_name: "j1".to_string(),
                    namespace: "default".into(),
                    exit_code: Some(0),
                    status: crate::meat::batch_tracker::JobStatus::Completed,
                },
            )])
            .await
            .unwrap();
        assert_eq!(ok[0], CouncilResponse::Applied { log_index: 2 });

        // …a conflicting terminal report is refused…
        let refused = sm
            .apply_fixture(vec![normal_entry(
                1,
                3,
                RaftRequest::BatchJobUpdate {
                    batch_id: 1,
                    job_name: "j1".to_string(),
                    namespace: "default".into(),
                    exit_code: Some(1),
                    status: crate::meat::batch_tracker::JobStatus::Failed,
                },
            )])
            .await
            .unwrap();
        assert!(matches!(refused[0], CouncilResponse::Refused { .. }));

        // …and so is a report for an unknown batch.
        let unknown = sm
            .apply_fixture(vec![normal_entry(
                1,
                4,
                RaftRequest::BatchJobUpdate {
                    batch_id: 99,
                    job_name: "j1".to_string(),
                    namespace: "default".into(),
                    exit_code: Some(0),
                    status: crate::meat::batch_tracker::JobStatus::Completed,
                },
            )])
            .await
            .unwrap();
        assert!(matches!(unknown[0], CouncilResponse::Refused { .. }));
    }

    #[tokio::test]
    async fn build_update_refuses_unknown_ids_and_bad_transitions() {
        let mut sm = CouncilStateMachine::new();
        sm.apply_fixture(vec![normal_entry(
            1,
            1,
            RaftRequest::BuildRegister {
                build: build_record("b1"),
            },
        )])
        .await
        .unwrap();

        let ok = sm
            .apply_fixture(vec![normal_entry(
                1,
                2,
                RaftRequest::BuildUpdate {
                    build_id: 1,
                    state: crate::bun::build_runner::BuildState::Failed {
                        reason: "boom".to_string(),
                    },
                },
            )])
            .await
            .unwrap();
        assert_eq!(ok[0], CouncilResponse::Applied { log_index: 2 });

        let refused = sm
            .apply_fixture(vec![
                normal_entry(
                    1,
                    3,
                    RaftRequest::BuildUpdate {
                        build_id: 1,
                        state: crate::bun::build_runner::BuildState::Completed {
                            image: "b1:v1".to_string(),
                        },
                    },
                ),
                normal_entry(
                    1,
                    4,
                    RaftRequest::BuildUpdate {
                        build_id: 42,
                        state: crate::bun::build_runner::BuildState::Running,
                    },
                ),
            ])
            .await
            .unwrap();
        assert!(matches!(refused[0], CouncilResponse::Refused { .. }));
        assert!(matches!(refused[1], CouncilResponse::Refused { .. }));
    }

    /// JOB7: attaching a signature to a digest the catalogue doesn't
    /// know is refused, not silently dropped — the old no-op let a
    /// build report success with a signature attached to nothing.
    #[tokio::test]
    async fn attach_signature_for_an_unknown_digest_is_refused() {
        let mut sm = CouncilStateMachine::new();
        let attach = crate::pickle::types::AttachSignature {
            manifest_digest: test_digest("unknown"),
            signature: crate::pickle::types::ImageSignature {
                method: crate::pickle::types::SigningMethod::ExternalKey {
                    key_id: "k".to_string(),
                },
                signature: "sig".to_string(),
                verification_material: crate::pickle::types::VerificationMaterial::PublicKey(vec![
                    1, 2, 3,
                ]),
                signed_at: std::time::SystemTime::UNIX_EPOCH,
            },
        };
        let responses = sm
            .apply_fixture(vec![normal_entry(
                1,
                1,
                RaftRequest::AttachSignature(attach),
            )])
            .await
            .unwrap();
        assert!(
            matches!(responses[0], CouncilResponse::Refused { .. }),
            "got: {:?}",
            responses[0]
        );
    }

    fn leased_token() -> crate::sesame::types::ApiToken {
        crate::sesame::types::ApiToken {
            name: "rbtest-run1-scope".into(),
            token_hash: vec![1; 32],
            token_salt: vec![2; 16],
            role: crate::sesame::types::ApiRole::Deployer,
            scope: crate::sesame::types::TokenScope {
                apps: None,
                namespaces: Some(vec!["rbtest-run1".into()]),
            },
            expires_at: Some(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(100),
            ),
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    fn leased_token_request(token: crate::sesame::types::ApiToken) -> RaftRequest {
        RaftRequest::TestLeaseApiToken {
            lease_id: "run1".into(),
            owner_id: "token:ci".into(),
            observed_at_unix_ms: 20,
            token: Box::new(token),
        }
    }

    #[test]
    fn leased_token_refuses_invalid_authority_scope_expiry_and_existing_names() {
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::TestLeaseCreate(test_lease("run1", 100)));
        let mut invalid = Vec::new();
        let mut token = leased_token();
        token.role = crate::sesame::types::ApiRole::Admin;
        invalid.push(token);
        let mut token = leased_token();
        token.scope.namespaces = None;
        invalid.push(token);
        let mut token = leased_token();
        token.scope.namespaces = Some(vec!["outside".into()]);
        invalid.push(token);
        let mut token = leased_token();
        token.expires_at = None;
        invalid.push(token);
        let mut token = leased_token();
        token.expires_at =
            Some(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(101));
        invalid.push(token);
        let mut token = leased_token();
        token.expires_at = Some(std::time::SystemTime::UNIX_EPOCH);
        invalid.push(token);
        let mut token = leased_token();
        token.name = "operator-token".into();
        invalid.push(token);
        for token in invalid {
            assert!(matches!(
                inner.apply_request(&leased_token_request(token)),
                Some(CouncilResponse::Refused { .. })
            ));
            assert!(inner.state.security_state.api_tokens.is_empty());
            assert!(inner.state.test_leases["run1"].resources.is_empty());
        }
        for (owner, observed) in [("other", 20), ("token:ci", 100)] {
            assert!(matches!(
                inner.apply_request(&RaftRequest::TestLeaseApiToken {
                    lease_id: "run1".into(),
                    owner_id: owner.into(),
                    observed_at_unix_ms: observed,
                    token: Box::new(leased_token()),
                }),
                Some(CouncilResponse::Refused { .. })
            ));
        }
        inner.state.security_state.api_tokens.push(leased_token());
        assert!(matches!(
            inner.apply_request(&leased_token_request(leased_token())),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(inner.state.test_leases["run1"].resources.is_empty());
    }

    #[test]
    fn leased_token_expiry_does_not_extend_on_renewal_and_revocation_does_not_release_ownership() {
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::TestLeaseCreate(test_lease("run1", 100)));
        inner.apply_request(&leased_token_request(leased_token()));
        inner.apply_request(&RaftRequest::TestLeaseRenew {
            lease_id: "run1".into(),
            owner_id: "token:ci".into(),
            renewed_at_unix_ms: 30,
            expires_at_unix_ms: 200,
        });
        assert_eq!(
            inner.state.security_state.api_tokens[0].expires_at,
            leased_token().expires_at
        );
        inner.apply_request(&RaftRequest::RevokeApiToken {
            name: leased_token().name,
        });
        assert!(inner.state.security_state.api_tokens.is_empty());
        assert!(matches!(
            inner.apply_request(&leased_token_request(leased_token())),
            Some(CouncilResponse::Refused { .. })
        ));
        let mut replacement = leased_token();
        replacement.name = "rbtest-run1-next".into();
        assert!(!matches!(
            inner.apply_request(&leased_token_request(replacement)),
            Some(CouncilResponse::Refused { .. })
        ));
    }

    #[tokio::test]
    async fn leased_token_snapshot_preserves_cleanup_fences_and_exact_credential() {
        let mut sm = CouncilStateMachine::new();
        sm.apply_fixture(vec![
            normal_entry(1, 1, RaftRequest::TestLeaseCreate(test_lease("run1", 100))),
            normal_entry(1, 2, leased_token_request(leased_token())),
        ])
        .await
        .unwrap();
        let state = sm.desired_state().await;
        let resource = state.test_leases["run1"]
            .resources
            .iter()
            .next()
            .unwrap()
            .clone();
        let crate::testkit::lease::LeasedResource::ApiToken { name, fingerprint } = resource else {
            panic!("wrong resource")
        };
        let mut builder = sm.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.unwrap();
        let mut restored = CouncilStateMachine::new();
        restored
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        let mut inner = restored.inner.write().await;
        let cleanup = RaftRequest::TestLeaseRevokeApiToken {
            lease_id: "run1".into(),
            name: name.clone(),
            fingerprint,
        };
        assert!(matches!(
            inner.apply_request(&cleanup),
            Some(CouncilResponse::Refused { .. })
        ));
        inner.apply_request(&RaftRequest::TestLeaseBeginCleanup {
            lease_id: "run1".into(),
        });
        assert!(matches!(
            inner.apply_request(&leased_token_request(leased_token())),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(matches!(
            inner.apply_request(&RaftRequest::TestLeaseFinishCleanup {
                lease_id: "run1".into()
            }),
            Some(CouncilResponse::Refused { .. })
        ));
        // Even corrupt/external replacement state must never cause name-only deletion.
        inner.state.security_state.api_tokens[0].token_hash = vec![3; 32];
        assert!(matches!(
            inner.apply_request(&cleanup),
            Some(CouncilResponse::Refused { .. })
        ));
        assert_eq!(
            inner.state.security_state.api_tokens[0].token_hash,
            vec![3; 32]
        );
        inner.state.security_state.api_tokens[0] = leased_token();
        assert!(!matches!(
            inner.apply_request(&cleanup),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(inner.state.security_state.api_tokens.is_empty());
        assert!(!matches!(
            inner.apply_request(&cleanup),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(matches!(
            inner.apply_request(&RaftRequest::CreateApiToken(leased_token())),
            Some(CouncilResponse::Refused { .. })
        ));
        inner.apply_request(&RaftRequest::TestLeaseFinishCleanup {
            lease_id: "run1".into(),
        });
        assert!(inner.state.test_leases.is_empty());
    }

    #[test]
    fn ordinary_manifest_commits_cannot_bypass_a_reserved_test_repository() {
        let mut inner = StateMachineInner::default();
        let mut commit = test_manifest_commit();
        commit.manifest.repository = "rbtest-run1/web".into();
        assert!(matches!(
            inner.apply_request(&RaftRequest::ManifestCommit(commit)),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(inner.state.manifest_catalog.manifests.is_empty());
    }

    #[test]
    fn registry_writer_receipts_fence_late_commits_and_delay_lease_completion() {
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::TestLeaseCreate(test_lease("run1", 100)));
        let writer = |node, owner: &str| {
            serde_json::from_value::<RaftRequest>(serde_json::json!({
                "TestLeaseRegistryWriter": {
                    "lease_id": "run1", "repository": "rbtest-run1/web", "node_id": node,
                    "owner_id": owner, "observed_at_unix_ms": 20
                }
            }))
            .expect("Raft must record a repository's possible storage owners")
        };
        assert!(matches!(
            inner.apply_request(&writer(1, "wrong")),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(inner.apply_request(&writer(1, "token:ci")).is_none());
        assert!(inner.apply_request(&writer(2, "token:ci")).is_none());
        assert!(inner.apply_request(&writer(1, "token:ci")).is_none());
        let mut commit = test_manifest_commit();
        commit.manifest.repository = "rbtest-run1/web".into();
        commit.manifest.pushed_by = 1;
        commit.holder_nodes = std::collections::BTreeSet::from([1]);
        let publish = serde_json::from_value::<RaftRequest>(serde_json::json!({
            "TestLeaseManifestCommit": {"lease_id":"run1", "observed_at_unix_ms":20, "commit":commit}
        })).unwrap();
        assert!(inner.apply_request(&publish).is_none());
        inner.apply_request(&RaftRequest::TestLeaseBeginCleanup {
            lease_id: "run1".into(),
        });
        assert!(matches!(
            inner.apply_request(&writer(3, "token:ci")),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(matches!(
            inner.apply_request(&publish),
            Some(CouncilResponse::Refused { .. })
        ));
        let finish = RaftRequest::TestLeaseFinishCleanup {
            lease_id: "run1".into(),
        };
        assert!(matches!(
            inner.apply_request(&finish),
            Some(CouncilResponse::Refused { .. })
        ));
        let ack = |node| {
            serde_json::from_value::<RaftRequest>(serde_json::json!({
            "TestLeaseRegistryRetired": {"lease_id":"run1", "repository":"rbtest-run1/web", "node_id":node}
        })).unwrap()
        };
        // Cleaning alone does not establish that workloads have stopped.
        assert!(matches!(
            inner.apply_request(&ack(1)),
            Some(CouncilResponse::Refused { .. })
        ));
        let ready = serde_json::from_value::<RaftRequest>(serde_json::json!({
            "TestLeaseWorkloadsRetired": {"lease_id":"run1"}
        }))
        .unwrap();
        assert!(inner.apply_request(&ready).is_none());
        assert!(inner.apply_request(&ack(1)).is_none());
        assert!(inner.apply_request(&ack(1)).is_none());
        assert!(matches!(
            inner.apply_request(&finish),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(inner.apply_request(&ack(2)).is_none());
        assert!(inner.apply_request(&finish).is_none());
        assert!(inner.state.test_leases.is_empty());
        assert!(inner.state.manifest_catalog.manifests.is_empty());
    }

    #[tokio::test]
    async fn registry_receipts_survive_snapshots_and_decommission_releases_only_the_retired_node() {
        let mut sm = CouncilStateMachine::new();
        let retired = crate::cluster::identity::raft_id_from_name("old-worker");
        let surviving = crate::cluster::identity::raft_id_from_name("worker");
        let request = |node| RaftRequest::TestLeaseRegistryWriter {
            lease_id: "run1".into(),
            repository: "rbtest-run1/web".into(),
            node_id: node,
            owner_id: Some("token:ci".into()),
            observed_at_unix_ms: 20,
        };
        for (index, request) in [
            RaftRequest::TestLeaseCreate(test_lease("run1", 100)),
            request(retired),
            request(surviving),
        ]
        .into_iter()
        .enumerate()
        {
            let response = sm
                .apply_fixture(vec![normal_entry(1, index as u64 + 1, request)])
                .await
                .unwrap();
            assert!(!matches!(response[0], CouncilResponse::Refused { .. }));
        }
        let mut builder = sm.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.unwrap();
        let mut restored = CouncilStateMachine::new();
        restored
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        let mut inner = restored.inner.write().await;
        assert_eq!(
            inner.state.test_leases["run1"].repositories["rbtest-run1/web"].len(),
            2
        );
        let retire = RaftRequest::DecommissionNode {
            node_id: "old-worker".into(),
            retired_by: "operator".into(),
            reason: "powered off".into(),
            retired_at_unix_ms: 30,
            membership_log_id: None,
        };
        assert!(matches!(
            inner.apply_request(&retire),
            Some(CouncilResponse::NodeDecommissioned { .. })
        ));
        assert_eq!(
            inner.state.test_leases["run1"].repositories["rbtest-run1/web"],
            std::collections::BTreeSet::from([surviving])
        );
        let audit = inner.state.security_state.crl.retired_nodes["old-worker"].clone();
        assert_eq!(audit.released_registry_writers["run1"], 1);
        inner.apply_request(&retire);
        assert_eq!(
            inner.state.security_state.crl.retired_nodes["old-worker"],
            audit
        );
        assert!(matches!(
            inner.apply_request(&request(retired)),
            Some(CouncilResponse::Refused { .. })
        ));
    }

    #[test]
    fn registry_cleanup_waits_for_desired_apps_and_every_former_placement() {
        use crate::testkit::lease::{LeasedPlacement, LeasedResource};
        let mut inner = StateMachineInner::default();
        let mut lease = test_lease("run1", 100);
        let app_id = AppId::new("web", "rbtest-run1");
        lease.resources.insert(LeasedResource::App {
            app_id: app_id.clone(),
        });
        lease.placements.insert(LeasedPlacement {
            app_id: app_id.clone(),
            node_id: NodeId::new("worker"),
        });
        inner.apply_request(&RaftRequest::TestLeaseCreate(lease));
        inner.state.apps.insert(app_id.clone(), default_spec());
        inner.apply_request(&RaftRequest::TestLeaseBeginCleanup {
            lease_id: "run1".into(),
        });
        let ready = RaftRequest::TestLeaseWorkloadsRetired {
            lease_id: "run1".into(),
        };
        assert!(matches!(
            inner.apply_request(&ready),
            Some(CouncilResponse::Refused { .. })
        ));
        inner.apply_request(&RaftRequest::AppDelete {
            app_id: app_id.clone(),
        });
        assert!(matches!(
            inner.apply_request(&ready),
            Some(CouncilResponse::Refused { .. })
        ));
        inner.apply_request(&RaftRequest::TestLeasePlacementRetired {
            lease_id: "run1".into(),
            placement: LeasedPlacement {
                app_id,
                node_id: NodeId::new("worker"),
            },
        });
        assert!(inner.apply_request(&ready).is_none());
    }

    #[test]
    fn ordinary_app_specs_cannot_acquire_leased_images() {
        for init in [false, true] {
            let mut inner = StateMachineInner::default();
            let mut spec = default_spec();
            if init {
                spec.init = vec![crate::config::app::InitContainerSpec {
                    image: Some("rbtest-run1/web:latest".into()),
                    command: vec![],
                }];
            } else {
                spec.image = Some("registry.example:5050/rbtest-run1/web:latest".into());
            }
            let result = inner.apply_request(&RaftRequest::AppSpec {
                app_id: AppId::new("ordinary", "default"),
                spec: Box::new(spec),
            });
            assert!(
                matches!(result, Some(CouncilResponse::Refused { .. })),
                "ordinary app acquired a disposable image (init={init})"
            );
            assert!(inner.state.apps.is_empty());
        }
    }

    #[test]
    fn leased_images_require_the_same_active_application_lease_and_registered_repository() {
        let mut inner = StateMachineInner::default();
        for id in ["run1", "run2"] {
            inner.apply_request(&RaftRequest::TestLeaseCreate(test_lease(id, 100)));
            inner.apply_request(&RaftRequest::TestLeaseRegistryWriter {
                lease_id: id.into(),
                repository: format!("rbtest-{id}/web"),
                node_id: 1,
                owner_id: Some("token:ci".into()),
                observed_at_unix_ms: 20,
            });
        }
        for init in [false, true] {
            for (image, allowed) in [
                ("nginx:latest", true),
                ("rbtest-run1/web:latest", true),
                ("registry.example:5050/rbtest-run1/web@sha256:content", true),
                ("rbtest-run2/web:latest", false),
                ("rbtest-run1/missing:latest", false),
            ] {
                let mut spec = default_spec();
                if init {
                    spec.init = vec![crate::config::app::InitContainerSpec {
                        image: Some(image.into()),
                        command: vec![],
                    }];
                } else {
                    spec.image = Some(image.into());
                }
                let result = inner.apply_request(&RaftRequest::TestLeaseAppSpec {
                    lease_id: "run1".into(),
                    observed_at_unix_ms: 20,
                    app_id: AppId::new(if init { "init" } else { "main" }, "rbtest-run1"),
                    spec: Box::new(spec),
                });
                assert_eq!(
                    result.is_none(),
                    allowed,
                    "{image}, init={init}: {result:?}"
                );
            }
        }
        let owned = &inner.state.test_leases["run1"];
        assert!(
            crate::testkit::lease::authorise_image_references(
                ["rbtest-run1/web:latest"],
                Some((owned, 100))
            )
            .is_err()
        );
        inner.apply_request(&RaftRequest::TestLeaseBeginCleanup {
            lease_id: "run1".into(),
        });
        let owned = &inner.state.test_leases["run1"];
        assert!(
            crate::testkit::lease::authorise_image_references(
                ["rbtest-run1/web:latest"],
                Some((owned, 20))
            )
            .is_err()
        );
    }

    fn test_lease(id: &str, expires_at_unix_ms: u64) -> crate::testkit::lease::TestLease {
        crate::testkit::lease::TestLease::new(
            id.to_string(),
            "token:ci".to_string(),
            "ci".to_string(),
            format!("rbtest-{id}"),
            10,
            expires_at_unix_ms,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn node_job_leases_never_enter_replicated_state() {
        let mut sm = CouncilStateMachine::new();
        let suffix = "0123456789abcdef0123456789abcdef";
        let lease = crate::testkit::lease::TestLease::new_scoped(
            format!("node-jobs-{suffix}"),
            "owner".into(),
            "owner".into(),
            format!("rbtest-node-{suffix}"),
            10,
            100,
            crate::testkit::lease::LeaseScope::NodeJobs,
        )
        .unwrap();
        let responses = sm
            .apply_fixture(vec![normal_entry(
                1,
                1,
                RaftRequest::TestLeaseCreate(lease),
            )])
            .await
            .unwrap();
        assert!(matches!(responses[0], CouncilResponse::Refused { .. }));
        assert!(sm.inner.read().await.state.test_leases.is_empty());
    }

    #[tokio::test]
    async fn leased_app_write_atomically_records_ownership() {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "rbtest-run1");
        let responses = sm
            .apply_fixture(vec![
                normal_entry(1, 1, RaftRequest::TestLeaseCreate(test_lease("run1", 100))),
                normal_entry(
                    1,
                    2,
                    RaftRequest::TestLeaseAppSpec {
                        lease_id: "run1".to_string(),
                        observed_at_unix_ms: 20,
                        app_id: app_id.clone(),
                        spec: Box::new(default_spec()),
                    },
                ),
            ])
            .await
            .unwrap();
        assert!(
            responses
                .iter()
                .all(|response| { matches!(response, CouncilResponse::Applied { .. }) })
        );
        let state = sm.desired_state().await;
        assert!(state.apps.contains_key(&app_id));
        assert!(state.test_leases["run1"].resources.contains(
            &crate::testkit::lease::LeasedResource::App {
                app_id: app_id.clone()
            }
        ));
    }

    #[tokio::test]
    async fn leased_namespace_write_atomically_records_ownership() {
        let mut sm = CouncilStateMachine::new();
        let responses = sm
            .apply_fixture(vec![
                normal_entry(1, 1, RaftRequest::TestLeaseCreate(test_lease("run1", 100))),
                normal_entry(
                    1,
                    2,
                    RaftRequest::TestLeaseNamespaceSpec {
                        lease_id: "run1".to_string(),
                        observed_at_unix_ms: 20,
                        name: "rbtest-run1".to_string(),
                        spec: Box::new(crate::config::NamespaceSpec {
                            cpu: None,
                            memory: None,
                            gpu: None,
                            max_apps: Some(1),
                            max_replicas: None,
                            secret_key: false,
                        }),
                    },
                ),
            ])
            .await
            .unwrap();
        assert!(
            responses
                .iter()
                .all(|response| matches!(response, CouncilResponse::Applied { .. }))
        );
        let state = sm.desired_state().await;
        assert_eq!(state.namespaces["rbtest-run1"].max_apps, Some(1));
        assert!(state.test_leases["run1"].resources.contains(
            &crate::testkit::lease::LeasedResource::Namespace {
                name: "rbtest-run1".to_string(),
            }
        ));
    }

    #[tokio::test]
    async fn leased_app_refuses_expiry_namespace_mismatch_and_existing_app() {
        let mut sm = CouncilStateMachine::new();
        let existing = AppId::new("existing", "rbtest-run1");
        // Model a pre-lease snapshot containing the now-reserved prefix.
        sm.inner
            .write()
            .await
            .state
            .apps
            .insert(existing.clone(), default_spec());
        sm.apply_fixture(vec![normal_entry(
            1,
            2,
            RaftRequest::TestLeaseCreate(test_lease("run1", 30)),
        )])
        .await
        .unwrap();
        let responses = sm
            .apply_fixture(vec![
                normal_entry(
                    1,
                    3,
                    RaftRequest::TestLeaseAppSpec {
                        lease_id: "run1".to_string(),
                        observed_at_unix_ms: 30,
                        app_id: AppId::new("late", "rbtest-run1"),
                        spec: Box::new(default_spec()),
                    },
                ),
                normal_entry(
                    1,
                    4,
                    RaftRequest::TestLeaseAppSpec {
                        lease_id: "run1".to_string(),
                        observed_at_unix_ms: 20,
                        app_id: AppId::new("wrong", "production"),
                        spec: Box::new(default_spec()),
                    },
                ),
                normal_entry(
                    1,
                    5,
                    RaftRequest::TestLeaseAppSpec {
                        lease_id: "run1".to_string(),
                        observed_at_unix_ms: 20,
                        app_id: existing,
                        spec: Box::new(default_spec()),
                    },
                ),
            ])
            .await
            .unwrap();
        assert!(
            responses
                .iter()
                .all(|response| { matches!(response, CouncilResponse::Refused { .. }) })
        );
    }

    #[tokio::test]
    async fn ordinary_writes_cannot_use_a_reserved_test_namespace() {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("probe", "rbtest-unleased");
        let responses = sm
            .apply_fixture(vec![
                normal_entry(
                    1,
                    1,
                    RaftRequest::NamespaceSpec {
                        name: "rbtest-unleased".to_string(),
                        spec: Box::new(crate::config::NamespaceSpec {
                            cpu: None,
                            memory: None,
                            gpu: None,
                            max_apps: Some(1),
                            max_replicas: None,
                            secret_key: false,
                        }),
                    },
                ),
                normal_entry(
                    1,
                    2,
                    RaftRequest::AppSpec {
                        app_id: app_id.clone(),
                        spec: Box::new(default_spec()),
                    },
                ),
            ])
            .await
            .unwrap();
        assert!(
            responses
                .iter()
                .all(|response| matches!(response, CouncilResponse::Refused { .. }))
        );
        let state = sm.desired_state().await;
        assert!(!state.namespaces.contains_key("rbtest-unleased"));
        assert!(!state.apps.contains_key(&app_id));
    }

    #[tokio::test]
    async fn replicated_lease_count_is_bounded() {
        let mut sm = CouncilStateMachine::new();
        let entries: Vec<_> = (0..crate::testkit::lease::MAX_ACTIVE_TEST_LEASES)
            .map(|index| {
                normal_entry(
                    1,
                    index as u64 + 1,
                    RaftRequest::TestLeaseCreate(test_lease(&format!("run{index}"), 100)),
                )
            })
            .collect();
        let responses = sm.apply_fixture(entries).await.unwrap();
        assert!(
            responses
                .iter()
                .all(|response| matches!(response, CouncilResponse::Applied { .. }))
        );
        let responses = sm
            .apply_fixture(vec![normal_entry(
                1,
                100,
                RaftRequest::TestLeaseCreate(test_lease("overflow", 100)),
            )])
            .await
            .unwrap();
        assert!(matches!(responses[0], CouncilResponse::Refused { .. }));
    }

    #[tokio::test]
    async fn interrupted_lease_cleanup_remains_replicated_until_finished() {
        let mut sm = CouncilStateMachine::new();
        sm.apply_fixture(vec![normal_entry(
            1,
            1,
            RaftRequest::TestLeaseCreate(test_lease("run1", 100)),
        )])
        .await
        .unwrap();
        sm.apply_fixture(vec![normal_entry(
            1,
            2,
            RaftRequest::TestLeaseBeginCleanup {
                lease_id: "run1".to_string(),
            },
        )])
        .await
        .unwrap();
        sm.apply_fixture(vec![normal_entry(
            1,
            3,
            RaftRequest::TestLeaseCleanupFailed {
                lease_id: "run1".to_string(),
                reason: "worker unavailable".to_string(),
            },
        )])
        .await
        .unwrap();
        assert!(matches!(
            sm.desired_state().await.test_leases["run1"].state,
            crate::testkit::lease::TestLeaseState::Cleaning {
                attempts: 1,
                last_error: Some(ref error),
            } if error == "worker unavailable"
        ));
        sm.apply_fixture(vec![normal_entry(
            1,
            4,
            RaftRequest::TestLeaseFinishCleanup {
                lease_id: "run1".to_string(),
            },
        )])
        .await
        .unwrap();
        assert!(sm.desired_state().await.test_leases.is_empty());
    }

    #[test]
    fn lease_placement_limit_refuses_new_owners_without_losing_existing_work() {
        use crate::testkit::lease::{LeasedPlacement, LeasedResource, MAX_LEASED_PLACEMENTS};
        let mut inner = StateMachineInner::default();
        let app_id = AppId::new("web", "rbtest-run1");
        let mut lease = test_lease("run1", 100);
        lease.resources.insert(LeasedResource::App {
            app_id: app_id.clone(),
        });
        lease.placements = (0..MAX_LEASED_PLACEMENTS)
            .map(|index| LeasedPlacement {
                app_id: app_id.clone(),
                node_id: NodeId::new(format!("worker-{index}")),
            })
            .collect();
        assert!(lease.validate().is_ok());
        inner.state.apps.insert(app_id.clone(), default_spec());
        inner.state.test_leases.insert("run1".into(), lease);
        let decision = |node: &str| {
            RaftRequest::SchedulingDecision(SchedulingDecision {
                app_id: app_id.clone(),
                placements: vec![Placement {
                    node_id: NodeId::new(node),
                    resources: Resources::new(1, 1, 0),
                    ordinal: 0,
                }],
            })
        };
        assert!(inner.apply_request(&decision("worker-0")).is_none());
        for rejected in ["", "overflow-worker"] {
            assert!(matches!(
                inner.apply_request(&decision(rejected)),
                Some(CouncilResponse::Refused { .. })
            ));
            assert_eq!(
                inner.state.scheduling[&app_id][0].node_id,
                NodeId::new("worker-0")
            );
            assert_eq!(
                inner.state.test_leases["run1"].placements.len(),
                MAX_LEASED_PLACEMENTS
            );
        }
    }

    #[test]
    fn decommission_resolves_only_the_fenced_nodes_fault_obligation() {
        use crate::smoker::{
            reservation::NodeFaultReservation,
            types::{FaultRequest, FaultType},
        };
        let mut inner = StateMachineInner::default();
        inner.state.node_fault_reservations.last_sequence = 1;
        inner.state.node_fault_reservations.active = Some(NodeFaultReservation {
            sequence: 1,
            boot_id: "old-boot".into(),
            cleanup_after_unix_ms: 100,
            request: FaultRequest {
                fault_type: FaultType::NodeKill {
                    kill_containers: false,
                },
                target_service: String::new(),
                namespace: None,
                target_instance: None,
                target_node: Some("old-worker".into()),
                duration: std::time::Duration::from_secs(30),
                injected_by: "operator".into(),
                reason: None,
                include_leader: true,
                override_safety: true,
                acknowledged: true,
            },
        });
        let retire = |node: &str| RaftRequest::DecommissionNode {
            node_id: node.into(),
            retired_by: "operator".into(),
            reason: "powered off".into(),
            retired_at_unix_ms: 30,
            membership_log_id: None,
        };
        assert!(matches!(
            inner.apply_request(&retire("other-worker")),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(inner.state.node_fault_reservations.active.is_some());
        let response = inner.apply_request(&retire("old-worker"));
        assert!(
            matches!(response, Some(CouncilResponse::NodeDecommissioned { .. })),
            "{response:?}"
        );
        assert!(inner.state.node_fault_reservations.active.is_none());
        assert_eq!(inner.state.node_fault_reservations.last_sequence, 1);
        let record =
            serde_json::to_value(&inner.state.security_state.crl.retired_nodes["old-worker"])
                .unwrap();
        assert_eq!(record["released_node_fault"], 1);
    }

    #[test]
    fn decommission_refuses_stale_membership_and_quorum_loss_without_mutation() {
        let mut inner = StateMachineInner::default();
        let members: std::collections::BTreeMap<_, _> = (1..=3)
            .map(|id| {
                (
                    id,
                    CouncilNodeInfo::new("127.0.0.1:9000".parse().unwrap(), format!("node-{id}")),
                )
            })
            .collect();
        inner.state.last_membership = StoredMembership::new(
            Some(log_id(1, 1)),
            Membership::new(vec![std::collections::BTreeSet::from([1, 2, 3])], members),
        );
        let request = |node: &str, observed| RaftRequest::DecommissionNode {
            node_id: node.into(),
            retired_by: "operator".into(),
            reason: "powered off".into(),
            retired_at_unix_ms: 30,
            membership_log_id: observed,
        };
        assert!(matches!(
            inner.apply_request(&request("node-3", None)),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(inner.state.security_state.crl.retired_nodes.is_empty());
        assert!(matches!(
            inner.apply_request(&request("node-3", Some(log_id(1, 1)))),
            Some(CouncilResponse::NodeDecommissioned { .. })
        ));
        assert!(matches!(
            inner.apply_request(&request("node-2", Some(log_id(1, 1)))),
            Some(CouncilResponse::Refused { .. })
        ));
        assert_eq!(inner.state.security_state.crl.retired_nodes.len(), 1);
        // A repeat keeps its original outcome even after membership has moved.
        assert!(matches!(
            inner.apply_request(&request("node-3", None)),
            Some(CouncilResponse::NodeDecommissioned { .. })
        ));
    }

    #[test]
    fn decommission_fences_registry_proposals_at_commit_time() {
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::DecommissionNode {
            node_id: "old-writer".into(),
            retired_by: "operator".into(),
            reason: "powered off".into(),
            retired_at_unix_ms: 30,
            membership_log_id: None,
        });
        let id = crate::cluster::identity::raft_id_from_name("old-writer");
        let mut commit = test_manifest_commit();
        commit.manifest.pushed_by = id;
        commit.holder_nodes = std::collections::BTreeSet::from([id]);
        for request in [
            RaftRequest::ManifestCommit(commit),
            RaftRequest::GcReport(crate::pickle::types::GcReport {
                node_id: id,
                deleted_layers: vec![test_digest("orphan")],
            }),
        ] {
            assert!(
                matches!(
                    inner.apply_request(&request),
                    Some(CouncilResponse::Refused { .. })
                ),
                "an in-flight proposal was accepted after its writer retired"
            );
        }
        assert!(inner.state.manifest_catalog.manifests.is_empty());
    }

    #[test]
    fn decommission_fences_join_tokens_and_renewal_serials_at_commit_time() {
        let mut inner = StateMachineInner::default();
        let (_, token) = crate::sesame::join::create_join_token(
            std::time::Duration::from_secs(60),
            "old-worker",
        )
        .unwrap();
        inner.apply_request(&RaftRequest::CreateJoinToken(token.clone()));
        inner.apply_request(&RaftRequest::DecommissionNode {
            node_id: "old-worker".into(),
            retired_by: "operator".into(),
            reason: "powered off".into(),
            retired_at_unix_ms: 30,
            membership_log_id: None,
        });
        for request in [
            RaftRequest::CreateJoinToken(token.clone()),
            RaftRequest::ConsumeJoinTokenForIssue {
                token_hash: token.token_hash,
            },
            RaftRequest::AllocateNodeSerial {
                node_id: "old-worker".into(),
            },
        ] {
            assert!(matches!(
                inner.apply_request(&request),
                Some(CouncilResponse::Refused { .. })
            ));
        }
        assert_eq!(inner.state.security_state.next_serial, 0);
        assert!(!inner.state.security_state.join_tokens[0].consumed);
        let (_, fresh) = crate::sesame::join::create_join_token(
            std::time::Duration::from_secs(60),
            "fresh-worker",
        )
        .unwrap();
        assert!(
            inner
                .apply_request(&RaftRequest::CreateJoinToken(fresh.clone()))
                .is_none()
        );
        assert!(matches!(
            inner.apply_request(&RaftRequest::ConsumeJoinTokenForIssue {
                token_hash: fresh.token_hash
            }),
            Some(CouncilResponse::JoinTokenConsumed { .. })
        ));
    }

    #[tokio::test]
    async fn decommission_resolves_all_node_owners_and_survives_snapshot_restoration() {
        use crate::testkit::lease::LeasedPlacement;
        let mut sm = CouncilStateMachine::new();
        let mut requests = Vec::new();
        for lease_id in ["run1", "run2"] {
            let app_id = AppId::new("web", format!("rbtest-{lease_id}"));
            requests.extend([
                RaftRequest::TestLeaseCreate(test_lease(lease_id, 100)),
                RaftRequest::TestLeaseAppSpec {
                    lease_id: lease_id.into(),
                    observed_at_unix_ms: 20,
                    app_id: app_id.clone(),
                    spec: Box::new(default_spec()),
                },
                RaftRequest::SchedulingDecision(SchedulingDecision {
                    app_id: app_id.clone(),
                    placements: ["retired-worker", "surviving-worker"]
                        .into_iter()
                        .zip(0..)
                        .map(|(node, ordinal)| Placement {
                            node_id: NodeId::new(node),
                            resources: Resources::new(1, 1, 0),
                            ordinal,
                        })
                        .collect(),
                }),
            ]);
        }
        requests.push(RaftRequest::TestLeaseBeginCleanup {
            lease_id: "run1".into(),
        });
        requests.push(RaftRequest::AppDelete {
            app_id: AppId::new("web", "rbtest-run1"),
        });
        for (index, request) in requests.into_iter().enumerate() {
            let response = sm
                .apply_fixture(vec![normal_entry(1, index as u64 + 1, request)])
                .await
                .unwrap();
            assert!(!matches!(response[0], CouncilResponse::Refused { .. }));
        }
        let request: RaftRequest = serde_json::from_value(serde_json::json!({
            "DecommissionNode": {"node_id":"retired-worker", "retired_by":"token:operator",
                "reason":"powered off for maintenance", "retired_at_unix_ms":30,
                "membership_log_id":null}
        }))
        .expect("Raft must expose durable node decommissioning");
        let response = sm
            .apply_fixture(vec![normal_entry(1, 20, request.clone())])
            .await
            .unwrap();
        assert!(!matches!(response[0], CouncilResponse::Refused { .. }));
        let mut builder = sm.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.unwrap();
        let mut restored = CouncilStateMachine::new();
        restored
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        let mut inner = restored.inner.write().await;
        for lease_id in ["run1", "run2"] {
            let lease = &inner.state.test_leases[lease_id];
            assert_eq!(lease.placements.len(), 1);
            assert_eq!(
                lease.placements.first().unwrap().node_id,
                NodeId::new("surviving-worker")
            );
        }
        let record = serde_json::to_value(&inner.state.security_state.crl).unwrap()["retired_nodes"]["retired-worker"].clone();
        assert_eq!(record["retired_by"], "token:operator");
        assert_eq!(
            record["released_placements"],
            serde_json::json!({"run1":1,"run2":1})
        );
        inner.apply_request(&request);
        assert_eq!(
            serde_json::to_value(&inner.state.security_state.crl).unwrap()["retired_nodes"]["retired-worker"],
            record
        );
        let app_id = AppId::new("web", "rbtest-run2");
        let schedule = |node| {
            RaftRequest::SchedulingDecision(SchedulingDecision {
                app_id: app_id.clone(),
                placements: vec![Placement {
                    node_id: NodeId::new(node),
                    resources: Resources::new(1, 1, 0),
                    ordinal: 0,
                }],
            })
        };
        assert!(matches!(
            inner.apply_request(&schedule("retired-worker")),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(
            inner
                .apply_request(&schedule("replacement-worker"))
                .is_none()
        );
        assert!(
            inner.state.test_leases["run2"]
                .placements
                .contains(&LeasedPlacement {
                    app_id,
                    node_id: NodeId::new("replacement-worker"),
                })
        );
    }

    #[tokio::test]
    async fn lease_cleanup_retains_former_placement_owners_after_rescheduling() {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "rbtest-run1");
        let schedule = |node: &str| {
            RaftRequest::SchedulingDecision(SchedulingDecision {
                app_id: app_id.clone(),
                placements: vec![Placement {
                    node_id: NodeId::new(node),
                    resources: Resources::new(500, 256 * 1024 * 1024, 0),
                    ordinal: 0,
                }],
            })
        };
        let requests = vec![
            RaftRequest::TestLeaseCreate(test_lease("run1", 100)),
            RaftRequest::TestLeaseAppSpec {
                lease_id: "run1".into(),
                observed_at_unix_ms: 20,
                app_id: app_id.clone(),
                spec: Box::new(default_spec()),
            },
            schedule("old-worker"),
            schedule("new-worker"),
            RaftRequest::TestLeaseBeginCleanup {
                lease_id: "run1".into(),
            },
            RaftRequest::AppDelete {
                app_id: app_id.clone(),
            },
        ];
        for (index, request) in requests.into_iter().enumerate() {
            let result = sm
                .apply_fixture(vec![normal_entry(1, index as u64 + 1, request)])
                .await
                .unwrap();
            assert!(
                !matches!(result[0], CouncilResponse::Refused { .. }),
                "{result:?}"
            );
        }
        let result = sm
            .apply_fixture(vec![normal_entry(
                1,
                7,
                RaftRequest::TestLeaseFinishCleanup {
                    lease_id: "run1".into(),
                },
            )])
            .await
            .unwrap();
        assert!(
            matches!(result[0], CouncilResponse::Refused { .. }),
            "runtime owners must outlive desired-state deletion: {result:?}"
        );
        assert!(sm.desired_state().await.test_leases.contains_key("run1"));
        let result = sm
            .apply_fixture(vec![normal_entry(1, 8, schedule("late-worker"))])
            .await
            .unwrap();
        assert!(
            matches!(result[0], CouncilResponse::Refused { .. }),
            "cleanup must fence stale scheduling decisions"
        );
        // A new leader must retain former owners even though desired placement
        // now contains neither node. Exercise the real snapshot codec.
        let mut builder = sm.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.unwrap();
        let mut restored = CouncilStateMachine::new();
        restored
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        let mut inner = restored.inner.write().await;
        assert_eq!(inner.state.test_leases["run1"].placements.len(), 2);
        let acknowledge = |lease: &str, node: &str| RaftRequest::TestLeasePlacementRetired {
            lease_id: lease.into(),
            placement: crate::testkit::lease::LeasedPlacement {
                app_id: app_id.clone(),
                node_id: NodeId::new(node),
            },
        };
        assert!(matches!(
            inner.apply_request(&acknowledge("another-lease", "old-worker")),
            Some(CouncilResponse::Refused { .. })
        ));
        assert_eq!(inner.state.test_leases["run1"].placements.len(), 2);
        assert!(
            inner
                .apply_request(&acknowledge("run1", "new-worker"))
                .is_none()
        );
        assert!(
            inner
                .apply_request(&acknowledge("run1", "new-worker"))
                .is_none()
        );
        assert!(matches!(
            inner.apply_request(&RaftRequest::TestLeaseFinishCleanup {
                lease_id: "run1".into()
            }),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(
            inner
                .apply_request(&acknowledge("run1", "old-worker"))
                .is_none()
        );
        assert!(
            inner
                .apply_request(&RaftRequest::TestLeaseFinishCleanup {
                    lease_id: "run1".into()
                })
                .is_none()
        );
        let mut replacement = test_lease("run2", 100);
        replacement.namespace = "rbtest-run1".into();
        assert!(
            inner
                .apply_request(&RaftRequest::TestLeaseCreate(replacement))
                .is_none()
        );
        assert!(
            inner
                .apply_request(&RaftRequest::TestLeaseAppSpec {
                    lease_id: "run2".into(),
                    observed_at_unix_ms: 20,
                    app_id: app_id.clone(),
                    spec: Box::new(default_spec())
                })
                .is_none()
        );
        assert!(inner.apply_request(&schedule("old-worker")).is_none());
        assert!(matches!(
            inner.apply_request(&acknowledge("run1", "old-worker")),
            Some(CouncilResponse::Refused { .. })
        ));
        assert!(matches!(
            inner.apply_request(&acknowledge("run2", "old-worker")),
            Some(CouncilResponse::Refused { .. })
        ));
        assert_eq!(inner.state.test_leases["run2"].placements.len(), 1);
    }

    /// The cleanup-snapshot race made executable. A cleanup driver snapshots
    /// the lease's resources, but an app can attach in the window before
    /// `TestLeaseBeginCleanup` commits. Re-reading desired state after
    /// BeginCleanup must show that app as owned, and `TestLeaseFinishCleanup`
    /// must refuse to destroy the record while the app still exists — otherwise
    /// the app leaks with no record left to reap it.
    #[tokio::test]
    async fn finish_cleanup_refuses_while_a_raced_app_still_exists() {
        let mut sm = CouncilStateMachine::new();
        // A driver would snapshot this lease's (empty) resource set here.
        sm.apply_fixture(vec![normal_entry(
            1,
            1,
            RaftRequest::TestLeaseCreate(test_lease("run1", 100)),
        )])
        .await
        .unwrap();

        // Race: an app attaches to the lease after that snapshot but before
        // cleanup begins. The lease is still Active, so the write is accepted.
        let app_id = AppId::new("web", "rbtest-run1");
        sm.apply_fixture(vec![normal_entry(
            1,
            2,
            RaftRequest::TestLeaseAppSpec {
                lease_id: "run1".to_string(),
                observed_at_unix_ms: 20,
                app_id: app_id.clone(),
                spec: Box::new(default_spec()),
            },
        )])
        .await
        .unwrap();

        // Cleanup begins; BeginCleanup commits and freezes the resource set.
        sm.apply_fixture(vec![normal_entry(
            1,
            3,
            RaftRequest::TestLeaseBeginCleanup {
                lease_id: "run1".to_string(),
            },
        )])
        .await
        .unwrap();

        // Re-reading desired state after BeginCleanup shows the raced app as
        // owned — this is the fresh view `cleanup_cluster_lease` now iterates.
        let state = sm.desired_state().await;
        assert!(state.test_leases["run1"].resources.contains(
            &crate::testkit::lease::LeasedResource::App {
                app_id: app_id.clone(),
            }
        ));
        assert!(state.apps.contains_key(&app_id));

        // Defence in depth: finishing while the app still exists is refused,
        // and the ownership record survives so the next attempt can retry.
        let responses = sm
            .apply_fixture(vec![normal_entry(
                1,
                4,
                RaftRequest::TestLeaseFinishCleanup {
                    lease_id: "run1".to_string(),
                },
            )])
            .await
            .unwrap();
        assert!(
            matches!(responses[0], CouncilResponse::Refused { .. }),
            "got: {:?}",
            responses[0]
        );
        assert!(
            sm.desired_state().await.test_leases.contains_key("run1"),
            "a refused finish must leave the ownership record durable"
        );

        // Resumed cleanup deletes the app (as the re-read set instructs), and
        // only then does FinishCleanup succeed and drop the record.
        sm.apply_fixture(vec![normal_entry(
            1,
            5,
            RaftRequest::AppDelete {
                app_id: app_id.clone(),
            },
        )])
        .await
        .unwrap();
        let responses = sm
            .apply_fixture(vec![normal_entry(
                1,
                6,
                RaftRequest::TestLeaseFinishCleanup {
                    lease_id: "run1".to_string(),
                },
            )])
            .await
            .unwrap();
        assert!(
            matches!(responses[0], CouncilResponse::Applied { .. }),
            "got: {:?}",
            responses[0]
        );
        let state = sm.desired_state().await;
        assert!(state.test_leases.is_empty());
        assert!(!state.apps.contains_key(&app_id));
    }

    /// O5: the CRL is replicated in every snapshot and scanned on every TLS
    /// handshake, and nothing ever removed an entry. An expired certificate
    /// fails validation with or without one, so those entries are pure
    /// growth.
    #[test]
    fn revoking_prunes_entries_whose_certificates_have_expired() {
        use crate::sesame::types::{CaRole, CrlEntry, SerialNumber};

        let epoch = std::time::SystemTime::UNIX_EPOCH;
        let at = |secs: u64| epoch + std::time::Duration::from_secs(secs);
        let entry = |serial: u64, revoked: u64, expires: Option<u64>| CrlEntry {
            serial: SerialNumber(serial),
            issuer: CaRole::Node,
            revoked_at: at(revoked),
            reason: format!("node-{serial}"),
            expires_at: expires.map(at),
        };

        let mut inner = StateMachineInner::default();
        // Expires at t=100, revoked early.
        inner.apply_request(&RaftRequest::RevokeCertificate(entry(1, 10, Some(100))));
        // No known expiry: never pruned, the safe direction to be wrong.
        inner.apply_request(&RaftRequest::RevokeCertificate(entry(2, 20, None)));
        assert_eq!(inner.state.security_state.crl.entries.len(), 2);

        // A revocation at t=200 is the logical clock: entry 1's certificate
        // expired 100 seconds ago, so it goes.
        inner.apply_request(&RaftRequest::RevokeCertificate(entry(3, 200, Some(900))));
        let serials: Vec<u64> = inner
            .state
            .security_state
            .crl
            .entries
            .iter()
            .map(|e| e.serial.0)
            .collect();
        assert_eq!(
            serials,
            vec![2, 3],
            "expected the expired entry to be pruned"
        );

        // The version still moves for every revocation — a pruning apply is
        // not a no-op to peers refreshing their copy.
        assert_eq!(inner.state.security_state.crl.version, 3);
    }

    /// The prune must use a timestamp carried in the log, never the wall
    /// clock: every replica applies the same entry, and reading `now()` here
    /// would have them prune different sets and diverge.
    #[test]
    fn crl_pruning_is_deterministic_across_replicas() {
        use crate::sesame::types::{CaRole, CrlEntry, SerialNumber};

        let epoch = std::time::SystemTime::UNIX_EPOCH;
        let at = |secs: u64| epoch + std::time::Duration::from_secs(secs);
        let requests = [
            RaftRequest::RevokeCertificate(CrlEntry {
                serial: SerialNumber(1),
                issuer: CaRole::Node,
                revoked_at: at(10),
                reason: "one".to_string(),
                expires_at: Some(at(50)),
            }),
            RaftRequest::RevokeCertificate(CrlEntry {
                serial: SerialNumber(2),
                issuer: CaRole::Node,
                revoked_at: at(60),
                reason: "two".to_string(),
                expires_at: Some(at(500)),
            }),
        ];

        let apply_all = || {
            let mut inner = StateMachineInner::default();
            for request in &requests {
                inner.apply_request(request);
            }
            inner
                .state
                .security_state
                .crl
                .entries
                .iter()
                .map(|e| e.serial.0)
                .collect::<Vec<_>>()
        };

        assert_eq!(apply_all(), apply_all());
        assert_eq!(apply_all(), vec![2]);
    }

    /// M12: `crl.updated_at` must come from the in-log `revoked_at`, not
    /// `SystemTime::now()`, or replicas store different values and the
    /// replicated state machines diverge on that field.
    #[test]
    fn crl_updated_at_is_the_in_log_timestamp() {
        use crate::sesame::types::{CaRole, CrlEntry, SerialNumber};

        let revoked_at = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1234);
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::RevokeCertificate(CrlEntry {
            serial: SerialNumber(1),
            issuer: CaRole::Node,
            revoked_at,
            reason: "one".to_string(),
            expires_at: None,
        }));
        assert_eq!(inner.state.security_state.crl.updated_at, revoked_at);
    }
    /// A changed request or hard selector keeps the app's assignments: the
    /// node agents roll each replica in place, and the scheduler moves only
    /// the ones whose node no longer admits the spec (#434). Dropping them
    /// here would retire every replica at once.
    #[tokio::test]
    async fn a_spec_change_keeps_its_assignments_so_replicas_roll_in_place() {
        let app_id = AppId::new("web", "default");
        let old: crate::config::app::AppSpec =
            toml::from_str("image='web:v1'\ncpu='600m'\n[placement]\nrequired=['zone=east']")
                .unwrap();
        for constraint in ["resources", "labels", "image"] {
            let mut sm = CouncilStateMachine::new();
            sm.apply_fixture([normal_entry(
                1,
                1,
                RaftRequest::AppSpec {
                    app_id: app_id.clone(),
                    spec: Box::new(old.clone()),
                },
            )])
            .await
            .unwrap();
            sm.apply_fixture([normal_entry(
                1,
                2,
                RaftRequest::SchedulingDecision(crate::meat::types::SchedulingDecision {
                    app_id: app_id.clone(),
                    placements: vec![crate::meat::types::Placement {
                        node_id: crate::meat::NodeId::new("east"),
                        resources: crate::meat::types::Resources::new(600, 0, 0),
                        ordinal: 0,
                    }],
                }),
            )])
            .await
            .unwrap();
            let mut updated = old.clone();
            match constraint {
                "resources" => updated.cpu.as_mut().unwrap().request = 2000,
                "labels" => updated.placement.as_mut().unwrap().required = vec!["zone=west".into()],
                _ => updated.image = Some("web:v2".into()),
            }
            sm.apply_fixture([normal_entry(
                1,
                3,
                RaftRequest::AppSpec {
                    app_id: app_id.clone(),
                    spec: Box::new(updated),
                },
            )])
            .await
            .unwrap();
            let state = sm.desired_state().await;
            assert!(
                state.scheduling.contains_key(&app_id),
                "a {constraint} change retired every replica at once"
            );
            assert!(state.last_placed_nodes.contains_key(&app_id));
        }
    }
    // Append inside src/council/state_machine.rs's existing tests module.
    // Prepared OFFTREE only. These snippets have NOT been compiled or executed.

    fn claim_review_manifest() -> crate::config::Config {
        crate::config::Config::parse(
        "[app.web]\nimage='web:v2'\n[job.migrate]\nimage='migration:v1'\nrun_before=['app.web']\n",
    )
    .unwrap()
    }

    #[tokio::test]
    async fn prerequisite_commit_rechecks_deleted_namespace_without_partial_publication() {
        let operation_id = "11111111111111111111111111111111".to_owned();
        let mut sm = CouncilStateMachine::new();
        let external = crate::config::Config::parse("[namespace.external]\n")
            .unwrap()
            .namespace
            .remove("external")
            .unwrap();
        sm.apply(vec![normal_entry(
            1,
            1,
            RaftRequest::NamespaceSpec {
                name: "external".into(),
                spec: Box::new(external),
            },
        )])
        .await
        .unwrap();
        let config = crate::config::Config::parse(
        "[app.web]\nimage='web:v2'\n[job.migrate]\nimage='migration:v1'\nrun_before=['app.web']\n[namespace.staged]\n[permission.staged]\nactions=['deploy']\nnamespaces=['external']\n",
    )
    .unwrap();
        let begun = sm
            .apply(vec![normal_entry(
                1,
                2,
                RaftRequest::PrerequisiteBegin {
                    operation_id: operation_id.clone(),
                    term: 1,
                    config: Box::new(config.clone()),
                },
            )])
            .await
            .unwrap();
        assert!(!matches!(&begun[0], CouncilResponse::Refused { .. }));
        sm.apply(vec![normal_entry(
            1,
            3,
            RaftRequest::NamespaceDelete {
                name: "external".into(),
            },
        )])
        .await
        .unwrap();
        let before = sm.desired_state().await;
        let committed = sm
            .apply(vec![normal_entry(
                1,
                4,
                RaftRequest::PrerequisiteCommit {
                    operation_id: operation_id.clone(),
                },
            )])
            .await
            .unwrap();
        assert!(matches!(&committed[0], CouncilResponse::Refused { .. }));
        let after = sm.desired_state().await;
        assert_eq!(after.namespaces, before.namespaces);
        assert_eq!(after.permissions, before.permissions);
        assert_eq!(after.apps, before.apps);
        assert_eq!(after.prerequisite_claims[&operation_id].config, config);
        assert!(!after.namespaces.contains_key("staged"));
        assert!(!after.permissions.contains_key("staged"));
    }

    async fn claim_review_active_app() -> (CouncilStateMachine, String, AppId) {
        let operation_id = "22222222222222222222222222222222".to_owned();
        let app_id = AppId::new("web", "default");
        let mut sm = CouncilStateMachine::new();
        sm.apply(vec![normal_entry(
            1,
            1,
            RaftRequest::AppSpec {
                app_id: app_id.clone(),
                spec: Box::new(default_spec()),
            },
        )])
        .await
        .unwrap();
        let begun = sm
            .apply(vec![normal_entry(
                1,
                2,
                RaftRequest::PrerequisiteBegin {
                    operation_id: operation_id.clone(),
                    term: 1,
                    config: Box::new(claim_review_manifest()),
                },
            )])
            .await
            .unwrap();
        assert!(!matches!(&begun[0], CouncilResponse::Refused { .. }));
        (sm, operation_id, app_id)
    }

    #[tokio::test]
    async fn prerequisite_claim_refuses_a_stop_that_its_later_commit_would_undo() {
        let (mut sm, operation_id, app_id) = claim_review_active_app().await;
        let before = sm.desired_state().await;
        let stopped = sm
            .apply(vec![normal_entry(
                1,
                3,
                RaftRequest::AppStop {
                    app_id: app_id.clone(),
                },
            )])
            .await
            .unwrap();
        assert!(matches!(&stopped[0], CouncilResponse::Refused { .. }));
        let after = sm.desired_state().await;
        assert_eq!(after.apps, before.apps);
        assert_eq!(after.stopped_apps, before.stopped_apps);
        assert!(after.prerequisite_claims.contains_key(&operation_id));
    }

    #[tokio::test]
    async fn prerequisite_claim_refuses_a_delete_that_its_later_commit_would_restore() {
        let (mut sm, operation_id, app_id) = claim_review_active_app().await;
        let before = sm.desired_state().await;
        let deleted = sm
            .apply(vec![normal_entry(
                1,
                3,
                RaftRequest::AppDelete {
                    app_id: app_id.clone(),
                },
            )])
            .await
            .unwrap();
        assert!(matches!(&deleted[0], CouncilResponse::Refused { .. }));
        let after = sm.desired_state().await;
        assert_eq!(after.apps, before.apps);
        assert_eq!(after.stopped_apps, before.stopped_apps);
        assert!(after.prerequisite_claims.contains_key(&operation_id));
    }

    #[tokio::test]
    async fn prerequisite_snapshot_and_restart_retain_the_original_claim_and_term_fence() {
        let operation_id = "33333333333333333333333333333333".to_owned();
        let config = claim_review_manifest();
        let root = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::create(root.path().join("claims.redb")).unwrap());
        let mut sm = CouncilStateMachine::with_store(db.clone()).unwrap();
        let begun = sm
            .apply(vec![normal_entry(
                1,
                1,
                RaftRequest::PrerequisiteBegin {
                    operation_id: operation_id.clone(),
                    term: 1,
                    config: Box::new(config.clone()),
                },
            )])
            .await
            .unwrap();
        assert!(!matches!(&begun[0], CouncilResponse::Refused { .. }));
        let snapshot = sm
            .get_snapshot_builder()
            .await
            .build_snapshot()
            .await
            .unwrap();
        let mut restored = CouncilStateMachine::new();
        restored
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        let restarted = CouncilStateMachine::with_store(db).unwrap();
        for state in [
            restored.desired_state().await,
            restarted.desired_state().await,
        ] {
            let held = &state.prerequisite_claims[&operation_id];
            assert_eq!(held.term, 1);
            assert_eq!(held.config, config);
        }
        for (index, request) in [
            RaftRequest::PrerequisiteCommit {
                operation_id: operation_id.clone(),
            },
            RaftRequest::PrerequisiteFailed {
                operation_id: operation_id.clone(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            let response = restored
                .apply(vec![normal_entry(2, index as u64 + 2, request)])
                .await
                .unwrap();
            assert!(matches!(&response[0], CouncilResponse::Refused { .. }));
            assert!(
                restored
                    .desired_state()
                    .await
                    .prerequisite_claims
                    .contains_key(&operation_id)
            );
        }
    }

    #[test]
    fn prerequisite_decode_refuses_missing_or_malformed_ownership_inventory() {
        let encoded = serde_json::to_value(DesiredState::default()).unwrap();
        for malformed in [
            None,
            Some(serde_json::Value::Null),
            Some(serde_json::json!([])),
            Some(serde_json::json!({"44444444444444444444444444444444": {"config": {}}})),
            Some(serde_json::json!({"44444444444444444444444444444444": {"term": 1}})),
        ] {
            let mut document = encoded.clone();
            match malformed {
                Some(inventory) => document["prerequisite_claims"] = inventory,
                None => {
                    document
                        .as_object_mut()
                        .unwrap()
                        .remove("prerequisite_claims");
                }
            }
            assert!(
                serde_json::from_value::<DesiredState>(document).is_err(),
                "missing or malformed durable ownership must never become an empty fence"
            );
        }
    }

    #[tokio::test]
    async fn ordinary_job_intent_blocks_a_same_namespace_prerequisite_but_not_another_namespace() {
        let mut sm = CouncilStateMachine::new();
        let ordinary =
            crate::config::Config::parse("[job.migrate]\nimage='migration:v1'\n").unwrap();
        let first = sm
            .apply(vec![normal_entry(
                1,
                1,
                RaftRequest::PrerequisiteBegin {
                    operation_id: "66666666666666666666666666666666".into(),
                    term: 1,
                    config: Box::new(ordinary),
                },
            )])
            .await
            .unwrap();
        assert!(
            !matches!(&first[0], CouncilResponse::Refused { .. }),
            "ordinary jobs need authoritative intent before asynchronous dispatch"
        );
        let collision = sm
            .apply(vec![normal_entry(
                1,
                2,
                RaftRequest::PrerequisiteBegin {
                    operation_id: "77777777777777777777777777777777".into(),
                    term: 1,
                    config: Box::new(claim_review_manifest()),
                },
            )])
            .await
            .unwrap();
        assert!(matches!(&collision[0], CouncilResponse::Refused { .. }));
        let separate = crate::config::Config::parse(
        "[namespace.other]\n[app.web]\nimage='web:v2'\nnamespace='other'\n[job.migrate]\nimage='migration:v1'\nnamespace='other'\nrun_before=['app.web']\n",
    )
    .unwrap();
        let allowed = sm
            .apply(vec![normal_entry(
                1,
                3,
                RaftRequest::PrerequisiteBegin {
                    operation_id: "88888888888888888888888888888888".into(),
                    term: 1,
                    config: Box::new(separate),
                },
            )])
            .await
            .unwrap();
        assert!(!matches!(&allowed[0], CouncilResponse::Refused { .. }));
        let state = sm.desired_state().await;
        assert_eq!(state.prerequisite_claims.len(), 2);
        assert!(
            !state
                .prerequisite_claims
                .contains_key("77777777777777777777777777777777")
        );
    }

    #[tokio::test]
    async fn a_recovered_prerequisite_cannot_complete_or_release_when_the_term_number_repeats() {
        let (sm, operation_id, _) = claim_review_active_app().await;
        let backup = sm.desired_state().await;
        let old_epoch = backup.recovery_epoch;
        assert_eq!(backup.prerequisite_claims[&operation_id].term, 1);
        for request in [
            RaftRequest::PrerequisiteCommit {
                operation_id: operation_id.clone(),
            },
            RaftRequest::PrerequisiteFailed {
                operation_id: operation_id.clone(),
            },
        ] {
            // Each branch gets a separate recovery: a bad successful Commit must
            // not hide the independent Failed-release defect by removing the claim.
            let mut recovered = CouncilStateMachine::from_recovered_state(backup.clone());
            assert!(recovered.desired_state().await.recovery_epoch > old_epoch);
            let response = recovered
                .apply(vec![normal_entry(1, 1, request)])
                .await
                .unwrap();
            assert!(
                matches!(&response[0], CouncilResponse::Refused { .. }),
                "a matching term number cannot reuse ownership from the dead cluster epoch"
            );
            let after = recovered.desired_state().await;
            assert_eq!(after.apps, backup.apps);
            assert_eq!(
                after.prerequisite_claims[&operation_id].config,
                claim_review_manifest()
            );
        }
    }

    #[test]
    fn prerequisite_decode_refuses_semantically_invalid_or_unbounded_claims() {
        let mut state = DesiredState::default();
        state.prerequisite_claims.insert(
            "66666666666666666666666666666666".into(),
            super::super::prerequisites::PrerequisiteClaim {
                term: 1,
                recovery_epoch: 0,
                apps_committed: false,
                config: claim_review_manifest(),
            },
        );
        let valid = serde_json::to_value(&state).unwrap();
        let claim = valid["prerequisite_claims"]["66666666666666666666666666666666"].clone();
        let mut malformed = Vec::new();
        malformed.push(serde_json::json!({"invalid-operation": claim.clone()}));
        let mut empty = claim.clone();
        empty["config"] = serde_json::json!({});
        malformed.push(serde_json::json!({"66666666666666666666666666666666": empty}));
        let mut too_many = serde_json::Map::new();
        for i in 0..65 {
            too_many.insert(format!("{i:032x}"), claim.clone());
        }
        malformed.push(serde_json::Value::Object(too_many));
        let mut too_large = claim;
        too_large["config"]["job"]["migrate"]["env"] = serde_json::json!({"PAYLOAD": serde_json::to_value(crate::config::types::EnvValue::Plain("x".repeat(8*1024*1024))).unwrap()});
        malformed.push(serde_json::json!({"66666666666666666666666666666666": too_large}));
        for inventory in malformed {
            let mut document = valid.clone();
            document["prerequisite_claims"] = inventory;
            assert!(
                serde_json::from_value::<DesiredState>(document).is_err(),
                "malformed ownership must refuse recovery"
            );
        }
    }

    #[tokio::test]
    async fn prerequisite_commit_releases_apps_and_migrations_but_retains_ordinary_job_names() {
        let mut sm = CouncilStateMachine::new();
        let operation_id = "99999999999999999999999999999999".to_owned();
        let config = crate::config::Config::parse(
        "[app.web]\nimage='web:v2'\n[job.migrate]\nimage='migration:v1'\nrun_before=['app.web']\n[job.notify]\nimage='notify:v1'\n",
    )
    .unwrap();
        let begun = sm
            .apply(vec![normal_entry(
                1,
                1,
                RaftRequest::PrerequisiteBegin {
                    operation_id: operation_id.clone(),
                    term: 1,
                    config: Box::new(config),
                },
            )])
            .await
            .unwrap();
        assert!(!matches!(&begun[0], CouncilResponse::Refused { .. }));
        let completed = sm
            .apply(vec![normal_entry(
                1,
                2,
                RaftRequest::PrerequisiteCommit {
                    operation_id: operation_id.clone(),
                },
            )])
            .await
            .unwrap();
        assert!(!matches!(&completed[0], CouncilResponse::Refused { .. }));
        let state = sm.desired_state().await;
        let held = state
            .prerequisite_claims
            .get(&operation_id)
            .expect("ordinary dispatch must retain the claim after desired-state commit");
        assert_eq!(serde_json::to_value(held).unwrap()["apps_committed"], true);
        assert!(!held.blocks("web", "default"));
        assert!(!held.blocks("migrate", "default"));
        assert!(held.blocks("notify", "default"));
        assert!(!held.blocks("notify", "another"));

        for (index, request) in [
            RaftRequest::AppStop {
                app_id: AppId::new("web", "default"),
            },
            RaftRequest::AppDelete {
                app_id: AppId::new("web", "default"),
            },
            RaftRequest::AppSpec {
                app_id: AppId::new("web", "default"),
                spec: Box::new(default_spec()),
            },
            // A completed migration no longer owns its old runtime identity.
            RaftRequest::AppSpec {
                app_id: AppId::new("migrate", "default"),
                spec: Box::new(default_spec()),
            },
        ]
        .into_iter()
        .enumerate()
        {
            let response = sm
                .apply(vec![normal_entry(1, index as u64 + 3, request)])
                .await
                .unwrap();
            assert!(!matches!(&response[0], CouncilResponse::Refused { .. }));
        }
        let collision = sm
            .apply(vec![normal_entry(
                1,
                7,
                RaftRequest::AppSpec {
                    app_id: AppId::new("notify", "default"),
                    spec: Box::new(default_spec()),
                },
            )])
            .await
            .unwrap();
        assert!(matches!(&collision[0], CouncilResponse::Refused { .. }));
        let second_commit = sm
            .apply(vec![normal_entry(
                1,
                8,
                RaftRequest::PrerequisiteCommit {
                    operation_id: operation_id.clone(),
                },
            )])
            .await
            .unwrap();
        assert!(matches!(&second_commit[0], CouncilResponse::Refused { .. }));
        let failed_release = sm
            .apply(vec![normal_entry(
                1,
                9,
                RaftRequest::PrerequisiteFailed {
                    operation_id: operation_id.clone(),
                },
            )])
            .await
            .unwrap();
        assert!(matches!(
            &failed_release[0],
            CouncilResponse::Refused { .. }
        ));
        assert!(
            sm.desired_state()
                .await
                .prerequisite_claims
                .contains_key(&operation_id)
        );
    }

    #[tokio::test]
    async fn prerequisite_decode_refuses_a_duplicate_operation_key_in_the_snapshot_json() {
        let (sm, operation_id, _) = claim_review_active_app().await;
        let state = sm.desired_state().await;
        let claim = serde_json::to_string(&state.prerequisite_claims[&operation_id]).unwrap();
        let inventory = format!("{{\"{operation_id}\":{claim}}}");
        let repeated = format!("{{\"{operation_id}\":{claim},\"{operation_id}\":{claim}}}");
        let document = serde_json::to_string(&state).unwrap();
        let needle = format!("\"prerequisite_claims\":{inventory}");
        assert_eq!(document.matches(&needle).count(), 1);
        let damaged = document.replacen(&needle, &format!("\"prerequisite_claims\":{repeated}"), 1);
        assert!(
            serde_json::from_str::<DesiredState>(&damaged).is_err(),
            "duplicate durable operation keys must not silently replace prior ownership"
        );
    }
    #[tokio::test]
    async fn prerequisite_decode_refuses_distinct_operations_owning_the_same_active_identity() {
        let (sm, operation_id, _) = claim_review_active_app().await;
        let state = sm.desired_state().await;
        let mut value = serde_json::to_value(&state).unwrap();
        let claim = value["prerequisite_claims"][&operation_id].clone();
        value["prerequisite_claims"]["ffffffffffffffffffffffffffffffff"] = claim;
        assert!(
            serde_json::from_value::<DesiredState>(value).is_err(),
            "distinct durable operations must not own the same active namespaced identity"
        );
    }
    async fn assert_oversized_claim_refuses_before_staging(config: crate::config::Config) {
        let mut sm = CouncilStateMachine::new();
        PREREQUISITE_NAMESPACE_VISITS.with(|visits| visits.set(0));
        let result = sm
            .apply(vec![normal_entry(
                1,
                1,
                RaftRequest::PrerequisiteBegin {
                    operation_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    term: 1,
                    config: Box::new(config),
                },
            )])
            .await
            .unwrap();
        assert!(
            matches!(&result[0], CouncilResponse::Refused { .. }),
            "{result:?}"
        );
        let state = sm.desired_state().await;
        assert!(state.prerequisite_claims.is_empty());
        assert!(state.namespaces.is_empty());
        let visits = PREREQUISITE_NAMESPACE_VISITS.with(|visits| visits.replace(0));
        assert_eq!(
            visits, 0,
            "oversized manifest performed desired-state staging before bounded admission"
        );
    }

    #[tokio::test]
    async fn prerequisite_resource_bound_refuses_before_any_staged_desired_write() {
        let mut config = claim_review_manifest();
        for i in 0..super::super::prerequisites::MAX_TARGETS {
            config.namespace.insert(
                format!("bounded-{i}"),
                toml::from_str::<crate::config::NamespaceSpec>("").unwrap(),
            );
        }
        assert_oversized_claim_refuses_before_staging(config).await;
    }

    #[tokio::test]
    async fn prerequisite_encoded_size_bound_refuses_before_any_staged_desired_write() {
        let mut config = claim_review_manifest();
        config.namespace.insert(
            "bounded".into(),
            toml::from_str::<crate::config::NamespaceSpec>("").unwrap(),
        );
        config.job.get_mut("migrate").unwrap().env.insert(
            "PAYLOAD".into(),
            crate::config::types::EnvValue::Plain(
                "x".repeat(super::super::prerequisites::MAX_CLAIM_BYTES),
            ),
        );
        assert_oversized_claim_refuses_before_staging(config).await;
    }
    #[tokio::test]
    async fn a_stale_sync_completion_cannot_consume_a_newer_admitted_webhook() {
        let mut machine = CouncilStateMachine::new();
        machine
            .apply(vec![normal_entry(
                1,
                1,
                RaftRequest::GitOpsSyncRequested { delivery: [1; 32] },
            )])
            .await
            .unwrap();
        let mut captured = machine.desired_state().await.gitops_sync_state.unwrap();
        machine
            .apply(vec![normal_entry(
                1,
                2,
                RaftRequest::GitOpsSyncRequested { delivery: [2; 32] },
            )])
            .await
            .unwrap();
        captured.completed_generation = captured.requested_generation;
        machine
            .apply(vec![normal_entry(
                1,
                3,
                RaftRequest::GitOpsSyncUpdate(Box::new(captured)),
            )])
            .await
            .unwrap();
        let state = machine.desired_state().await.gitops_sync_state.unwrap();
        assert_eq!(
            (state.requested_generation, state.completed_generation),
            (2, 1)
        );
        assert_eq!(state.webhook_receipts.len(), 2);
        let refused = machine
            .apply(vec![normal_entry(
                1,
                4,
                RaftRequest::GitOpsSyncRequested { delivery: [2; 32] },
            )])
            .await
            .unwrap();
        assert!(
            matches!(&refused[0], CouncilResponse::Refused { reason } if reason.contains("replay"))
        );
    }

    #[tokio::test]
    async fn admitted_webhooks_and_their_watch_survive_snapshot_install_and_restart() {
        let root = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::create(root.path().join("state.redb")).unwrap());
        let mut machine = CouncilStateMachine::with_store(db.clone()).unwrap();
        let mut updates = machine.gitops_trigger_updates();
        machine
            .apply(vec![normal_entry(
                1,
                1,
                RaftRequest::GitOpsSyncRequested { delivery: [7; 32] },
            )])
            .await
            .unwrap();
        updates.changed().await.unwrap();
        assert_eq!(*updates.borrow(), (1, 0));
        let snapshot = machine
            .get_snapshot_builder()
            .await
            .build_snapshot()
            .await
            .unwrap();
        let mut follower = CouncilStateMachine::new();
        let mut followed = follower.gitops_trigger_updates();
        follower
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        followed.changed().await.unwrap();
        assert_eq!(*followed.borrow(), (1, 0));
        let restarted = CouncilStateMachine::with_store(db).unwrap();
        assert_eq!(*restarted.gitops_trigger_updates().borrow(), (1, 0));
        assert_eq!(
            restarted
                .desired_state()
                .await
                .gitops_sync_state
                .unwrap()
                .webhook_receipts,
            std::collections::VecDeque::from([[7; 32]])
        );
    }

    #[test]
    fn webhook_receipts_are_bounded_and_generation_exhaustion_refuses_admission() {
        let mut inner = StateMachineInner::default();
        for index in 1u64..=1001 {
            let mut delivery = [0; 32];
            delivery[..8].copy_from_slice(&index.to_be_bytes());
            assert!(
                matches!(inner.apply_request(&RaftRequest::GitOpsSyncRequested { delivery }), Some(CouncilResponse::GitOpsSyncRequested { generation }) if generation == index)
            );
        }
        let state = inner.state.gitops_sync_state.as_mut().unwrap();
        assert_eq!(state.webhook_receipts.len(), 1000);
        state.requested_generation = u64::MAX;
        let before = state.clone();
        assert!(matches!(
            inner.apply_request(&RaftRequest::GitOpsSyncRequested {
                delivery: [255; 32]
            }),
            Some(CouncilResponse::Refused { .. })
        ));
        assert_eq!(inner.state.gitops_sync_state.unwrap(), before);
    }
}

// Append as a cfg(test) sibling module to src/council/state_machine.rs AFTER #543 integration.
// Uses the actual RaftStateMachine apply path; no new production helper is needed to compile.
#[cfg(test)]
mod audit_held_job_admission {
    use super::*;
    use crate::meat::batch_tracker::{BatchJobRecord, BatchRecord, JobStatus};
    use crate::meat::{NodeId, Resources};

    fn entry(term: u64, index: u64, request: RaftRequest) -> openraft::Entry<TypeConfig> {
        openraft::Entry {
            log_id: LogId::new(openraft::CommittedLeaderId::new(term, 0), index),
            payload: EntryPayload::Normal(request),
        }
    }
    fn manifest() -> crate::config::Config {
        crate::config::Config::parse("[app.web]\nimage='web:v1'\n[job.migrate]\nimage='migration:v1'\ncpu='5'\nrun_before=['app.web']\n[job.notify]\nimage='notify:v1'\ncpu='3'\n").unwrap()
    }
    fn empty_report(node: &str) -> crate::reporting::types::StateReport {
        crate::reporting::types::StateReport {
            node_id: NodeId::new(node),
            timestamp: std::time::SystemTime::UNIX_EPOCH,
            running_apps: vec![],
            cached_specs: vec![],
            event_log: vec![],
            has_buildah: false,
            resource_usage: Default::default(),
        }
    }
    fn footprint(state: &DesiredState, node: &str) -> u64 {
        crate::meat::admission::unreported_commitments(
            state,
            &NodeId::new(node),
            &empty_report(node),
        )
        .cpu_millicores
    }
    async fn begin(sm: &mut CouncilStateMachine, index: u64) -> String {
        let id = "1234567890abcdef1234567890abcdef".to_owned();
        let result = sm
            .apply([entry(
                1,
                index,
                RaftRequest::PrerequisiteBegin {
                    operation_id: id.clone(),
                    term: 1,
                    config: Box::new(manifest()),
                },
            )])
            .await
            .unwrap();
        assert!(
            !matches!(result[0], CouncilResponse::Refused { .. }),
            "{result:?}"
        );
        id
    }
    fn record(logical: &str, execution: &str, namespace: &str, time: u64) -> BatchRecord {
        BatchRecord {
            submitted_at_epoch_secs: time,
            jobs: vec![BatchJobRecord {
                name: logical.into(),
                execution_name: execution.into(),
                namespace: namespace.into(),
                spec_digest: "a".repeat(64),
                resources: Resources::default(),
                node: Some(NodeId::new("remote")),
                status: JobStatus::Pending,
            }],
        }
    }
    async fn register(
        sm: &mut CouncilStateMachine,
        index: u64,
        batch: BatchRecord,
    ) -> CouncilResponse {
        let previous = sm.desired_state().await.last_applied_log;
        sm.apply([entry(
            1,
            index,
            RaftRequest::BatchRegister {
                expected_log_id: previous,
                batch,
            },
        )])
        .await
        .unwrap()
        .remove(0)
    }

    #[tokio::test]
    async fn held_job_capacity_tracks_real_begin_commit_and_positive_settlement() {
        let mut sm = CouncilStateMachine::new();
        let id = begin(&mut sm, 1).await;
        let pending = sm.desired_state().await;
        for node in ["a", "b", "unreported-third"] {
            assert_eq!(
                footprint(&pending, node),
                8000,
                "precommit footprint on {node}"
            );
        }
        let result = sm
            .apply([entry(
                1,
                2,
                RaftRequest::PrerequisiteCommit {
                    operation_id: id.clone(),
                },
            )])
            .await
            .unwrap();
        assert!(
            !matches!(result[0], CouncilResponse::Refused { .. }),
            "{result:?}"
        );
        let committed = sm.desired_state().await;
        assert!(committed.prerequisite_claims[&id].apps_committed);
        for node in ["a", "b"] {
            assert_eq!(footprint(&committed, node), 3000);
        }
        let result = sm
            .apply([entry(
                1,
                3,
                RaftRequest::JobApplyComplete { operation_id: id },
            )])
            .await
            .unwrap();
        assert!(
            !matches!(result[0], CouncilResponse::Refused { .. }),
            "{result:?}"
        );
        assert_eq!(footprint(&sm.desired_state().await, "a"), 0);
    }

    #[tokio::test]
    async fn held_job_capacity_survives_snapshot_and_handover_without_a_guessed_release() {
        for commit_first in [false, true] {
            let mut sm = CouncilStateMachine::new();
            let id = begin(&mut sm, 1).await;
            if commit_first {
                let result = sm
                    .apply([entry(
                        1,
                        2,
                        RaftRequest::PrerequisiteCommit {
                            operation_id: id.clone(),
                        },
                    )])
                    .await
                    .unwrap();
                assert!(!matches!(result[0], CouncilResponse::Refused { .. }));
            }
            let snapshot = sm
                .get_snapshot_builder()
                .await
                .build_snapshot()
                .await
                .unwrap();
            let mut restored = CouncilStateMachine::new();
            restored
                .install_snapshot(&snapshot.meta, snapshot.snapshot)
                .await
                .unwrap();
            restored
                .apply([entry(2, 3, RaftRequest::Noop)])
                .await
                .unwrap();
            let result = restored
                .apply([entry(
                    2,
                    4,
                    if commit_first {
                        RaftRequest::JobApplyComplete {
                            operation_id: id.clone(),
                        }
                    } else {
                        RaftRequest::PrerequisiteFailed {
                            operation_id: id.clone(),
                        }
                    },
                )])
                .await
                .unwrap();
            assert!(
                matches!(result[0], CouncilResponse::Refused { .. }),
                "{result:?}"
            );
            let state = restored.desired_state().await;
            assert!(state.prerequisite_claims.contains_key(&id));
            for node in ["a", "b"] {
                assert_eq!(
                    footprint(&state, node),
                    if commit_first { 3000 } else { 8000 }
                );
            }
        }
    }

    #[tokio::test]
    async fn a_current_revision_batch_cannot_take_a_held_logical_job_but_other_namespace_is_independent()
     {
        let mut sm = CouncilStateMachine::new();
        let id = begin(&mut sm, 1).await;
        for (index, logical) in [(2, "migrate"), (3, "notify")] {
            let before = sm.desired_state().await.batch_state;
            let response = register(
                &mut sm,
                index,
                record(logical, &format!("execution-{index}"), "default", 1),
            )
            .await;
            assert!(
                matches!(response, CouncilResponse::Refused { .. }),
                "{response:?}"
            );
            assert_eq!(
                sm.desired_state().await.batch_state,
                before,
                "refusal mutated ownership/ID"
            );
        }
        let response = register(&mut sm, 4, record("notify", "execution-other", "other", 1)).await;
        assert!(
            matches!(response, CouncilResponse::BatchRegistered { .. }),
            "{response:?}"
        );
        let result = sm
            .apply([entry(
                1,
                5,
                RaftRequest::PrerequisiteCommit { operation_id: id },
            )])
            .await
            .unwrap();
        assert!(!matches!(result[0], CouncilResponse::Refused { .. }));
        let response = register(
            &mut sm,
            6,
            record("migrate", "execution-migration-released", "default", 1),
        )
        .await;
        assert!(
            matches!(response, CouncilResponse::BatchRegistered { .. }),
            "{response:?}"
        );
        let before = sm.desired_state().await.batch_state;
        let response = register(
            &mut sm,
            7,
            record("notify", "execution-ordinary-held", "default", 1),
        )
        .await;
        assert!(
            matches!(response, CouncilResponse::Refused { .. }),
            "{response:?}"
        );
        assert_eq!(sm.desired_state().await.batch_state, before);
    }

    #[tokio::test]
    async fn prerequisite_begin_cannot_claim_a_pruned_global_batch_physical_identity() {
        let mut sm = CouncilStateMachine::new();
        let physical = "batch-retained-physical";
        let response = register(
            &mut sm,
            1,
            record("logical-original", physical, "default", 1),
        )
        .await;
        let CouncilResponse::BatchRegistered { batch_id } = response else {
            panic!("{response:?}")
        };
        let result = sm
            .apply([entry(
                1,
                2,
                RaftRequest::BatchJobUpdate {
                    batch_id,
                    job_name: physical.into(),
                    namespace: "default".into(),
                    status: JobStatus::Completed,
                    exit_code: Some(0),
                },
            )])
            .await
            .unwrap();
        assert!(
            !matches!(result[0], CouncilResponse::Refused { .. }),
            "{result:?}"
        );
        let response = register(
            &mut sm,
            3,
            record(
                "pruning-trigger",
                "new-independent-physical",
                "default",
                7200,
            ),
        )
        .await;
        assert!(matches!(response, CouncilResponse::BatchRegistered { .. }));
        let before = sm.desired_state().await;
        assert!(
            before.batch_state.get(batch_id).is_none(),
            "fixture failed to prune history"
        );
        assert!(
            before
                .batch_state
                .execution_owner("default", physical)
                .is_some()
        );
        let config =
            crate::config::Config::parse(&format!("[job.'{physical}']\nimage='migration:v1'\n"))
                .unwrap();
        let response = sm
            .apply([entry(
                1,
                4,
                RaftRequest::PrerequisiteBegin {
                    operation_id: "abcdefabcdefabcdefabcdefabcdefab".into(),
                    term: 1,
                    config: Box::new(config.clone()),
                },
            )])
            .await
            .unwrap();
        assert!(
            matches!(response[0], CouncilResponse::Refused { .. }),
            "{response:?}"
        );
        assert_eq!(
            sm.desired_state().await.prerequisite_claims,
            before.prerequisite_claims
        );
        let mut independent = config;
        independent.job.get_mut(physical).unwrap().namespace = Some("other".into());
        let response = sm
            .apply([entry(
                1,
                5,
                RaftRequest::PrerequisiteBegin {
                    operation_id: "fedcba9876543210fedcba9876543210".into(),
                    term: 1,
                    config: Box::new(independent),
                },
            )])
            .await
            .unwrap();
        assert!(
            !matches!(response[0], CouncilResponse::Refused { .. }),
            "{response:?}"
        );
    }

    async fn begin_cross_namespace_same_label_claim(sm: &mut CouncilStateMachine) {
        let config = crate::config::Config::parse(
            "[app.foo]\nimage='web:v1'\nnamespace='team'\n[job.foo]\nimage='notify:v1'\nnamespace='other'\n",
        )
        .unwrap();
        config.validate_intrinsic().unwrap();
        let result = sm
            .apply([entry(
                1,
                1,
                RaftRequest::PrerequisiteBegin {
                    operation_id: "1234567890abcdef1234567890abcdef".into(),
                    term: 1,
                    config: Box::new(config),
                },
            )])
            .await
            .unwrap();
        assert!(
            !matches!(result[0], CouncilResponse::Refused { .. }),
            "fixture claim was refused: {result:?}"
        );
        assert_eq!(sm.desired_state().await.prerequisite_claims.len(), 1);
    }

    #[tokio::test]
    async fn cross_namespace_same_label_claim_does_not_block_independent_batch_display_label() {
        let mut sm = CouncilStateMachine::new();
        begin_cross_namespace_same_label_claim(&mut sm).await;
        let response = register(
            &mut sm,
            2,
            record("foo", "opaque-independent-job", "team", 1),
        )
        .await;
        assert!(
            matches!(response, CouncilResponse::BatchRegistered { .. }),
            "an other-namespace held job must not turn the team app display label into a held job: {response:?}"
        );
        assert!(
            sm.desired_state()
                .await
                .batch_state
                .execution_owner("team", "opaque-independent-job")
                .is_some()
        );
    }

    #[tokio::test]
    async fn cross_namespace_same_label_claim_still_blocks_its_held_job_operation() {
        let mut sm = CouncilStateMachine::new();
        begin_cross_namespace_same_label_claim(&mut sm).await;
        let before = sm.desired_state().await.batch_state;
        let response = register(
            &mut sm,
            2,
            record("foo", "opaque-independent-job", "other", 1),
        )
        .await;
        assert!(
            matches!(response, CouncilResponse::Refused { .. }),
            "the actual held job namespace must remain fenced: {response:?}"
        );
        assert_eq!(sm.desired_state().await.batch_state, before);
    }

    #[tokio::test]
    async fn cross_namespace_same_label_claim_still_blocks_held_app_physical_identity() {
        let mut sm = CouncilStateMachine::new();
        begin_cross_namespace_same_label_claim(&mut sm).await;
        let before = sm.desired_state().await.batch_state;
        let response = register(&mut sm, 2, record("independent-label", "foo", "team", 1)).await;
        assert!(
            matches!(response, CouncilResponse::Refused { .. }),
            "the held app physical identity must remain fenced: {response:?}"
        );
        assert_eq!(sm.desired_state().await.batch_state, before);
    }

    #[tokio::test]
    async fn a_batch_display_label_matching_a_held_app_is_independent() {
        let mut sm = CouncilStateMachine::new();
        begin(&mut sm, 1).await;
        let response = register(
            &mut sm,
            2,
            record("web", "opaque-independent-job", "default", 1),
        )
        .await;
        assert!(
            matches!(response, CouncilResponse::BatchRegistered { .. }),
            "{response:?}"
        );
        assert!(
            sm.desired_state()
                .await
                .batch_state
                .execution_owner("default", "opaque-independent-job")
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_batch_physical_execution_matching_a_held_app_is_refused() {
        let mut sm = CouncilStateMachine::new();
        begin(&mut sm, 1).await;
        let before = sm.desired_state().await.batch_state;
        let response = register(&mut sm, 2, record("independent-label", "web", "default", 1)).await;
        assert!(
            matches!(response, CouncilResponse::Refused { .. }),
            "{response:?}"
        );
        assert_eq!(sm.desired_state().await.batch_state, before);
    }
}
