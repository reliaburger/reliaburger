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
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
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

/// `replicas` held inside an `[autoscale]` block's `[min, max]`. A `min`
/// above `max` can't reach the log (validation refuses it), but the clamp
/// mustn't panic on one.
fn clamp_to_bounds(replicas: u32, autoscale: &crate::config::app::AutoscaleSpec) -> u32 {
    replicas.max(autoscale.min).min(autoscale.max)
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
                LeasedResource::Job { job_id } => {
                    self.state
                        .task_arrays
                        .jobs()
                        .definition(&job_id.namespace, &job_id.name)
                        .is_none()
                        && self
                            .state
                            .task_arrays
                            .deployment_owner(&job_id.name, &job_id.namespace)
                            .is_none()
                }
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
            RaftRequest::JobApplyCommit {
                operation_id,
                now_epoch_secs,
            } => {
                let Some(record) = self
                    .state
                    .task_arrays
                    .deployments()
                    .find_map(|(id, record)| (id == operation_id).then_some(record.clone()))
                else {
                    return Some(CouncilResponse::Refused {
                        reason: "unknown deployment".into(),
                    });
                };
                if record.cancelled {
                    return Some(CouncilResponse::Refused {
                        reason: "deployment was cancelled".into(),
                    });
                }
                if record.apps_committed {
                    return None;
                }
                let namespaces: Vec<_> = self.state.namespaces.keys().cloned().collect();
                if let Err(error) = record.config.validate_against(&namespaces) {
                    return Some(CouncilResponse::Refused {
                        reason: error.to_string(),
                    });
                }
                let mut staged = StateMachineInner {
                    state: self.state.clone(),
                    ..Default::default()
                };
                let transition = RaftRequest::TaskArray(Box::new(
                    crate::meat::task_array_store::TaskArrayWrite::DeployCommitted {
                        operation_id: operation_id.clone(),
                        now_epoch_secs: *now_epoch_secs,
                    },
                ));
                if let Some(CouncilResponse::Refused { reason }) =
                    staged.apply_request_at(&transition, position)
                {
                    return Some(CouncilResponse::Refused { reason });
                }
                for write in super::config_to_desired_writes(&record.config) {
                    if let Some(CouncilResponse::Refused { reason }) =
                        staged.apply_request_at(&write, position)
                    {
                        return Some(CouncilResponse::Refused { reason });
                    }
                }
                self.state = staged.state;
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
                    .task_arrays
                    .job_identity_reserved(&app_id.namespace, &app_id.name)
                {
                    return Some(CouncilResponse::Refused {
                        reason: "identity belongs to a job definition or retained run".into(),
                    });
                }
                if self
                    .state
                    .task_arrays
                    .deployment_owner(&app_id.name, &app_id.namespace)
                    .is_some()
                {
                    return Some(CouncilResponse::Refused {
                        reason: "an outstanding job deployment owns this app".into(),
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
                    .task_arrays
                    .deployment_owner(&app_id.name, &app_id.namespace)
                    .is_some()
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
                // A live lease's apps leave only through its cleanup, which
                // carries the lease (#693): GitOps or a stray delete can't
                // take a running test's workload out from under it.
                if self.namespace_is_leased(&app_id.namespace) {
                    return Some(CouncilResponse::Refused {
                        reason: format!(
                            "app {app_id} belongs to a test lease; only the lease's cleanup \
                             deletes it"
                        ),
                    });
                }
                if let Some(refused) = self.remove_app(app_id) {
                    return Some(refused);
                }
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
            RaftRequest::ExpireManifest(expire) => {
                // Recheck at apply time: a push, tag or deploy may have
                // landed between the leader's plan and this entry.
                let pinned = self.state.pinned_image_digests();
                if let Err(reason) = self
                    .state
                    .manifest_catalog
                    .apply_expire_manifest(expire, &pinned)
                {
                    return Some(CouncilResponse::Refused { reason });
                }
            }
            RaftRequest::AutoscaleOverride {
                app_id,
                replicas,
                reason: _,
            } => {
                // The autoscaler read a spec, decided, then wrote: a deploy
                // that removed `[autoscale]` or narrowed it may have
                // committed in between (#701).
                let Some(autoscale) = self
                    .state
                    .apps
                    .get(app_id)
                    .and_then(|spec| spec.autoscale.as_ref())
                else {
                    return Some(CouncilResponse::Refused {
                        reason: format!("app {app_id} doesn't autoscale"),
                    });
                };
                let replicas = clamp_to_bounds(*replicas, autoscale);
                let key = app_id.to_string();
                if let Some((_, existing)) = self
                    .state
                    .autoscale_overrides
                    .iter_mut()
                    .find(|(k, _)| k == &key)
                {
                    *existing = replicas;
                } else {
                    self.state.autoscale_overrides.push((key, replicas));
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
            RaftRequest::RotateApiToken(rotation) => {
                // A leased test token's cleanup matches it by its hash, so a
                // rotation would orphan it; those stay with their lease.
                // Unlike a revoke, a rotation may touch the last Admin: the
                // store keeps the same Admin, only its secret changes.
                if rotation.name.starts_with("rbtest-") {
                    return Some(CouncilResponse::Refused {
                        reason: "test-lease tokens can't be rotated".into(),
                    });
                }
                let Some(token) = self
                    .state
                    .security_state
                    .api_tokens
                    .iter_mut()
                    .find(|token| token.name == rotation.name)
                else {
                    return Some(CouncilResponse::Refused {
                        reason: format!("no API token named {:?}", rotation.name),
                    });
                };
                crate::sesame::token::apply_rotation(token, rotation);
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
            RaftRequest::RolloutFailed { app_id, failure } => {
                let current = self
                    .state
                    .apps
                    .get(app_id)
                    .map(crate::meat::deploy_types::rollout_fingerprint);
                if current.as_deref() != Some(failure.fingerprint.as_str()) {
                    return Some(CouncilResponse::Refused {
                        reason: format!(
                            "the failed rollout is not of {app_id}'s current spec; nothing to mark"
                        ),
                    });
                }
                let rollouts = self.state.rollouts.entry(app_id.clone()).or_default();
                // A node redeploying a version that already runs healthy
                // everywhere (its share moved while the cluster was full)
                // hasn't found a bad version. There's no previous spec to
                // hold the app on: it is the previous spec. The node backs
                // off and tries again.
                if rollouts
                    .last_released()
                    .map(crate::meat::deploy_types::rollout_fingerprint)
                    .as_deref()
                    == Some(failure.fingerprint.as_str())
                {
                    return Some(CouncilResponse::Refused {
                        reason: format!(
                            "{app_id}'s spec already rolled out on every node; \
                             a node's failed redeploy of it is not a rollout failure"
                        ),
                    });
                }
                // The first failure is the one that explains it. Another
                // node's report of the same spec adds nothing.
                if rollouts.failed_for(&failure.fingerprint).is_none() {
                    let mut failure = failure.clone();
                    failure.reason = failure.reason.chars().take(512).collect();
                    rollouts.failed = Some(failure);
                }
            }
            RaftRequest::RolloutReleased { app_id, spec } => {
                let fingerprint = crate::meat::deploy_types::rollout_fingerprint(spec);
                let current = self
                    .state
                    .apps
                    .get(app_id)
                    .map(crate::meat::deploy_types::rollout_fingerprint);
                if current.as_deref() != Some(fingerprint.as_str()) {
                    return Some(CouncilResponse::Refused {
                        reason: format!(
                            "the released spec is not {app_id}'s current spec; not a rollback target"
                        ),
                    });
                }
                let rollouts = self.state.rollouts.entry(app_id.clone()).or_default();
                // Running healthy everywhere is the answer to a failure report.
                if rollouts.failed_for(&fingerprint).is_some() {
                    rollouts.failed = None;
                }
                rollouts.release(spec);
            }
            RaftRequest::QuotaBlocked { blocked } => {
                // Only apps still in desired state: a delete committed after
                // the leader planned must not leave a ghost reason behind.
                self.state.quota_blocked = blocked
                    .iter()
                    .filter(|(app_id, _)| self.state.apps.contains_key(app_id))
                    .cloned()
                    .collect();
            }
            RaftRequest::Unschedulable { apps } => {
                // As for `QuotaBlocked`: no reason outlives its app.
                self.state.unschedulable = apps
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
                // A bun upgrade and an OS rollout both restart nodes; never
                // start one while the other holds the fleet.
                if self.state.os_rollout.is_some()
                    && self.state.active_upgrade.as_ref().map(|a| &a.upgrade_id)
                        != Some(&state.upgrade_id)
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
            RaftRequest::OsRolloutUpdate { rollout } => {
                // The same guards as a bun upgrade's (M13): no different
                // rollout over a running one (a resume replaces a paused
                // one), and none while a bun upgrade holds the fleet.
                if self.state.active_upgrade.is_some() {
                    return None;
                }
                if let Some(active) = &self.state.os_rollout
                    && active.rollout_id != rollout.rollout_id
                    && !matches!(
                        active.phase,
                        crate::os::rollout::OsRolloutPhase::Paused { .. }
                    )
                {
                    return None;
                }
                self.state.os_rollout = Some(*rollout.clone());
            }
            RaftRequest::CouncilSize { voters } => {
                self.state.council_size = Some(*voters);
            }
            RaftRequest::OsRolloutClear { rollout_id } => {
                if let Some(active) = self.state.os_rollout.take() {
                    if active.rollout_id == *rollout_id {
                        self.state.os_rollout_history.push(active);
                        if self.state.os_rollout_history.len() > 20 {
                            self.state.os_rollout_history.remove(0);
                        }
                    } else {
                        self.state.os_rollout = Some(active);
                    }
                }
            }
            RaftRequest::TaskArray(write) => {
                use crate::meat::task_array_store::TaskArrayWrite;
                if let TaskArrayWrite::DeployBegin {
                    config,
                    now_epoch_secs,
                    ..
                } = write.as_ref()
                {
                    // A job in a test namespace runs only under the active job
                    // lease that owns it, so the lease's cleanup can find it.
                    let leased_at = now_epoch_secs.saturating_mul(1000);
                    for (name, spec) in &config.job {
                        let namespace = spec.namespace.as_deref().unwrap_or("default");
                        if !crate::testkit::lease::valid_test_namespace(namespace) {
                            continue;
                        }
                        let resource = crate::testkit::lease::LeasedResource::Job {
                            job_id: crate::meat::AppId::new(name, namespace),
                        };
                        if !self.state.test_leases.values().any(|lease| {
                            lease.resources.contains(&resource) && lease.is_active_at(leased_at)
                        }) {
                            return Some(CouncilResponse::Refused {
                                reason: "a job in a test lease namespace needs an active job \
                                         lease that owns it"
                                    .into(),
                            });
                        }
                    }
                    let namespaces: Vec<_> = self.state.namespaces.keys().cloned().collect();
                    if let Err(error) = config.validate_against(&namespaces) {
                        return Some(CouncilResponse::Refused {
                            reason: error.to_string(),
                        });
                    }
                    let mut staged = StateMachineInner {
                        state: self.state.clone(),
                        ..Default::default()
                    };
                    for (name, spec) in &config.job {
                        let namespace = spec.namespace.as_deref().unwrap_or("default");
                        if self
                            .state
                            .apps
                            .contains_key(&crate::meat::AppId::new(name, namespace))
                            || config.app.get(name).is_some_and(|app| {
                                app.namespace.as_deref().unwrap_or("default") == namespace
                            })
                        {
                            return Some(CouncilResponse::Refused {
                                reason: "job identity belongs to an app".into(),
                            });
                        }
                    }
                    // Existing replayed ownership is ignored only for this preview;
                    // the real transaction still verifies the complete identity.
                    let op = match write.as_ref() {
                        TaskArrayWrite::DeployBegin { operation_id, .. } => operation_id.as_str(),
                        _ => "",
                    };
                    let mut preview = config.as_ref().clone();
                    preview.app.retain(|name, spec| {
                        self.state
                            .task_arrays
                            .deployment_owner(name, spec.namespace.as_deref().unwrap_or("default"))
                            .is_none_or(|owner| owner != op)
                    });
                    for desired in super::config_to_desired_writes(&preview) {
                        if let Some(CouncilResponse::Refused { reason }) =
                            staged.apply_request_at(&desired, position)
                        {
                            return Some(CouncilResponse::Refused { reason });
                        }
                    }
                }
                if let TaskArrayWrite::Job(job) = write.as_ref() {
                    let (name, namespace) = match job.as_ref() {
                        crate::meat::job::JobWrite::Put {
                            name, namespace, ..
                        }
                        | crate::meat::job::JobWrite::Fire {
                            name, namespace, ..
                        } => (name, namespace),
                    };
                    if self
                        .state
                        .apps
                        .contains_key(&crate::meat::AppId::new(name, namespace))
                    {
                        return Some(CouncilResponse::Refused { reason: "identity belongs to an app; delete it before changing workload kind".into() });
                    }
                }
                // Arrays share the batch id counter, so `batch-status ID`
                // names one thing. Every rule lives in `TaskArrays::apply`;
                // a refused write leaves the state untouched.
                let ids = match self.state.task_arrays.planned_ids(write) {
                    Ok(ids) => ids,
                    Err(error) => {
                        return Some(CouncilResponse::Refused {
                            reason: error.to_string(),
                        });
                    }
                };
                let batch_ids = &mut self.state.batch_ids;
                if let Err(reason) = batch_ids.preflight(ids) {
                    return Some(CouncilResponse::Refused { reason });
                }
                match self.state.task_arrays.apply(write, || batch_ids.allocate()) {
                    Ok(crate::meat::task_array_store::TaskArrayApplied::Registered {
                        batch_id,
                    }) => return Some(CouncilResponse::TaskArrayRegistered { batch_id }),
                    Ok(crate::meat::task_array_store::TaskArrayApplied::Ticked { refused }) => {
                        return Some(CouncilResponse::TaskArrayTicked { refused });
                    }
                    Ok(_) => {}
                    Err(error) => {
                        return Some(CouncilResponse::Refused {
                            reason: error.to_string(),
                        });
                    }
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
                if self.namespace_is_leased(name) {
                    return Some(CouncilResponse::Refused {
                        reason: format!(
                            "namespace {name} belongs to a test lease; only the lease's \
                             cleanup deletes it"
                        ),
                    });
                }
                self.state.namespaces.remove(name);
                self.state.gitops_owned.namespaces.remove(name);
            }
            RaftRequest::PermissionSpec { name, spec } => {
                self.state.permissions.insert(name.clone(), *spec.clone());
            }
            RaftRequest::PermissionDelete { name } => {
                self.state.permissions.remove(name);
                self.state.gitops_owned.permissions.remove(name);
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
            RaftRequest::TestLeaseJobs {
                lease_id,
                observed_at_unix_ms,
                jobs,
            } => {
                use crate::testkit::lease::{LeaseScope, LeasedResource};
                let refuse = |reason: &str| {
                    Some(CouncilResponse::Refused {
                        reason: reason.into(),
                    })
                };
                let Some(lease) = self.state.test_leases.get(lease_id) else {
                    return refuse("lease not found");
                };
                if !lease.is_active_at(*observed_at_unix_ms) {
                    return refuse("lease is expired or cleanup has started");
                }
                if lease.scope != LeaseScope::NodeJobs {
                    return refuse("only a job lease can own jobs");
                }
                if jobs.iter().any(|job| job.namespace != lease.namespace) {
                    return refuse("job namespace does not match its lease");
                }
                let resources: std::collections::BTreeSet<_> = jobs
                    .iter()
                    .map(|job_id| LeasedResource::Job {
                        job_id: job_id.clone(),
                    })
                    .collect();
                let additional = resources.difference(&lease.resources).count();
                if lease.resource_count().saturating_add(additional)
                    > crate::testkit::lease::MAX_LEASED_RESOURCES
                {
                    return refuse("lease resource limit reached");
                }
                if self.state.test_leases.iter().any(|(id, candidate)| {
                    id != lease_id && resources.iter().any(|r| candidate.resources.contains(r))
                }) {
                    return refuse("job already belongs to another lease");
                }
                if let Some(lease) = self.state.test_leases.get_mut(lease_id) {
                    lease.resources.extend(resources);
                }
            }
            RaftRequest::TestLeaseAppSpec {
                lease_id,
                observed_at_unix_ms,
                app_id,
                spec,
            } => {
                if self
                    .state
                    .task_arrays
                    .deployment_owner(&app_id.name, &app_id.namespace)
                    .is_some()
                {
                    return Some(CouncilResponse::Refused {
                        reason: "an outstanding job deployment owns this app".into(),
                    });
                }

                let Some(lease) = self.state.test_leases.get(lease_id) else {
                    return Some(CouncilResponse::Refused {
                        reason: "lease not found".to_string(),
                    });
                };
                if lease.scope != crate::testkit::lease::LeaseScope::Applications {
                    return Some(CouncilResponse::Refused {
                        reason: "a job lease owns only jobs".into(),
                    });
                }
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
                if lease.scope != crate::testkit::lease::LeaseScope::Applications {
                    return Some(CouncilResponse::Refused {
                        reason: "a job lease owns only jobs".into(),
                    });
                }
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
                if lease.scope != crate::testkit::lease::LeaseScope::Applications {
                    return Some(CouncilResponse::Refused {
                        reason: "a job lease owns only jobs".into(),
                    });
                }
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
                // Its image copies will never serve again: heal must see
                // every image it held as under-replicated (#697).
                self.state
                    .manifest_catalog
                    .remove_node_holdings(registry_node_id);
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
                        crate::testkit::lease::LeasedResource::Job { job_id }
                            if self
                                .state
                                .task_arrays
                                .jobs()
                                .definition(&job_id.namespace, &job_id.name)
                                .is_some()
                                || self
                                    .state
                                    .task_arrays
                                    .deployment_owner(&job_id.name, &job_id.namespace)
                                    .is_some() =>
                        {
                            Some(format!("job {job_id}"))
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
            RaftRequest::GitOpsWrite { term, change } => {
                return self.apply_gitops_write(*term, change, position);
            }
            RaftRequest::GitOpsConfigSet { config, seed } => {
                if *seed && self.state.gitops_config.is_some() {
                    return Some(CouncilResponse::Refused {
                        reason: "the cluster already has a GitOps configuration".into(),
                    });
                }
                self.state.gitops_config = config.as_deref().cloned();
            }
            RaftRequest::TestLeaseAppDelete { lease_id, app_id } => {
                if !self
                    .state
                    .test_leases
                    .get(lease_id)
                    .is_some_and(|lease| lease.namespace == app_id.namespace)
                {
                    return Some(CouncilResponse::Refused {
                        reason: "app is not in this lease's namespace".into(),
                    });
                }
                if let Some(refused) = self.remove_app(app_id) {
                    return Some(refused);
                }
            }
            RaftRequest::TestLeaseNamespaceDelete { lease_id, name } => {
                if !self
                    .state
                    .test_leases
                    .get(lease_id)
                    .is_some_and(|lease| &lease.namespace == name)
                {
                    return Some(CouncilResponse::Refused {
                        reason: "namespace does not belong to this lease".into(),
                    });
                }
                self.state.namespaces.remove(name);
                self.state.gitops_owned.namespaces.remove(name);
            }
        }
        None
    }

    /// Apply a Lettuce write and record what GitOps owns, in one entry (#693).
    ///
    /// Refused when the entry wasn't proposed in the term the sync read its
    /// snapshot in (finding 7), when the change isn't a declarative spec or
    /// delete, and when it deletes a resource GitOps didn't write (D9).
    fn apply_gitops_write(
        &mut self,
        term: u64,
        change: &RaftRequest,
        position: ApplyEntryPosition,
    ) -> Option<CouncilResponse> {
        let entry_term = position.current_log_id.map(|id| id.leader_id.get_term());
        if entry_term != Some(term) {
            return Some(CouncilResponse::Refused {
                reason: format!(
                    "the GitOps sync read desired state in term {term} but wrote in term \
                     {}; leadership changed, so the sync is abandoned",
                    entry_term.map_or_else(|| "unknown".to_string(), |t| t.to_string())
                ),
            });
        }
        let owned = &self.state.gitops_owned;
        let unowned = match change {
            RaftRequest::AppSpec { .. }
            | RaftRequest::NamespaceSpec { .. }
            | RaftRequest::PermissionSpec { .. } => None,
            RaftRequest::AppDelete { app_id } => {
                (!owned.apps.contains(app_id)).then(|| format!("app {app_id}"))
            }
            RaftRequest::NamespaceDelete { name } => {
                (!owned.namespaces.contains(name)).then(|| format!("namespace {name}"))
            }
            RaftRequest::PermissionDelete { name } => {
                (!owned.permissions.contains(name)).then(|| format!("permission {name}"))
            }
            _ => {
                return Some(CouncilResponse::Refused {
                    reason: "a GitOps write carries only app, namespace and permission changes"
                        .into(),
                });
            }
        };
        if let Some(resource) = unowned {
            return Some(CouncilResponse::Refused {
                reason: format!("GitOps didn't write {resource}, so it won't delete it"),
            });
        }
        let response = self.apply_request_at(change, position);
        if matches!(response, Some(CouncilResponse::Refused { .. })) {
            return response;
        }
        // Deletes forget ownership in their own handlers, as a manual
        // delete does; a spec is GitOps-owned from this write on.
        let owned = &mut self.state.gitops_owned;
        match change {
            RaftRequest::AppSpec { app_id, .. } => {
                owned.apps.insert(app_id.clone());
            }
            RaftRequest::NamespaceSpec { name, .. } => {
                owned.namespaces.insert(name.clone());
            }
            RaftRequest::PermissionSpec { name, .. } => {
                owned.permissions.insert(name.clone());
            }
            _ => {}
        }
        response
    }

    /// Whether a live test lease owns `namespace`.
    fn namespace_is_leased(&self, namespace: &str) -> bool {
        self.state
            .test_leases
            .values()
            .any(|lease| lease.namespace == namespace)
    }

    /// Remove an app and everything desired state keeps for it, unless an
    /// outstanding job deployment owns it.
    fn remove_app(&mut self, app_id: &crate::meat::types::AppId) -> Option<CouncilResponse> {
        if self
            .state
            .task_arrays
            .deployment_owner(&app_id.name, &app_id.namespace)
            .is_some()
        {
            return Some(CouncilResponse::Refused {
                reason: "an outstanding job operation owns this app".into(),
            });
        }
        self.state.apps.remove(app_id);
        self.state.stopped_apps.remove(app_id);
        self.state.rollouts.remove(app_id);
        self.state.scheduling.remove(app_id);
        self.state.last_placed_nodes.remove(app_id);
        self.state.quota_blocked.remove(app_id);
        self.state.unschedulable.remove(app_id);
        self.state.gitops_owned.apps.remove(app_id);
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
        let key = app_id.to_string();
        if baseline_changed {
            self.state.autoscale_overrides.retain(|(k, _)| k != &key);
        }
        // An override lives inside the `[autoscale]` bounds. Removing the
        // block leaves nothing that could ever move it again, so it goes;
        // narrowing the bounds clamps it (#701).
        match &spec.autoscale {
            None => self.state.autoscale_overrides.retain(|(k, _)| k != &key),
            Some(autoscale) => {
                for (_, replicas) in self
                    .state
                    .autoscale_overrides
                    .iter_mut()
                    .filter(|(k, _)| k == &key)
                {
                    *replicas = clamp_to_bounds(*replicas, autoscale);
                }
            }
        }
        // A different spec is the operator's answer to a failed rollout
        // (#692, D11-B). The same spec again, a GitOps re-sync say, isn't:
        // it would only fail the same way. "Different" means any change,
        // a scale change included (#929): the version's fingerprint sets
        // replicas aside, but an operator who changed the spec expects a
        // new rollout, not the old failure.
        let identical = self.state.apps.get(app_id) == Some(spec);
        self.state.apps.insert(app_id.clone(), spec.clone());
        if !identical && let Some(rollouts) = self.state.rollouts.get_mut(app_id) {
            rollouts.failed = None;
        }
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
        state.enter_recovery_epoch(0);

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
    /// `DesiredState` into the durable snapshot store at `snapshot_db`. The
    /// caller has already entered the new recovery epoch
    /// ([`DesiredState::enter_recovery_epoch`]), which needs the recovering
    /// node's fence record as its floor (#694). The next node start loads
    /// this snapshot via [`CouncilStateMachine::with_store`] and
    /// re-bootstraps a fresh Raft.
    ///
    /// The write uses the same enveloped format (version + checksum) as a
    /// normal snapshot, so nothing downstream can tell a recovered snapshot
    /// from an ordinary one except by the bumped `recovery_epoch`.
    // `redb::Error` is large but dictated by the crate; boxing it buys nothing.
    #[allow(clippy::result_large_err)]
    pub fn persist_recovered_snapshot(
        snapshot_db: &Database,
        state: DesiredState,
    ) -> Result<(), redb::Error> {
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
                    if let EntryPayload::Normal(RaftRequest::SchedulingDecision(decision)) =
                        &mut entry.payload
                    {
                        entry.payload = EntryPayload::Normal(RaftRequest::SchedulingDecisions {
                            expected_log_id: previous,
                            decisions: vec![decision.clone()],
                        });
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

    fn web_spec(image: &str) -> Box<AppSpec> {
        Box::new(toml::from_str(&format!("image = {image:?}\nreplicas = 3\n")).unwrap())
    }

    fn failure_of(spec: &AppSpec) -> crate::meat::deploy_types::RolloutFailure {
        crate::meat::deploy_types::RolloutFailure {
            fingerprint: crate::meat::deploy_types::rollout_fingerprint(spec),
            node: "node-2".into(),
            reason: "web-g1-0 failed its health check".into(),
            at_unix_ms: 1,
        }
    }

    /// An app whose rollout of `image` failed on a node.
    fn failed_rollout(image: &str) -> (StateMachineInner, AppId) {
        let mut inner = StateMachineInner::default();
        let app_id = AppId::new("web", "default");
        let spec = web_spec(image);
        inner.apply_request(&RaftRequest::AppSpec {
            app_id: app_id.clone(),
            spec: spec.clone(),
        });
        assert_eq!(
            inner.apply_request(&RaftRequest::RolloutFailed {
                app_id: app_id.clone(),
                failure: failure_of(&spec),
            }),
            None
        );
        (inner, app_id)
    }

    #[test]
    fn identical_spec_reapply_keeps_rollout_failed() {
        let (mut inner, app_id) = failed_rollout("web:v2");
        // A GitOps re-sync applies the very same spec again.
        inner.apply_request(&RaftRequest::AppSpec {
            app_id: app_id.clone(),
            spec: web_spec("web:v2"),
        });
        let failed = inner.state.rollouts[&app_id].failed.clone();
        assert_eq!(failed, Some(failure_of(&web_spec("web:v2"))));
    }

    /// #929: a scale change keeps the version, so the mark used to stay,
    /// and the operator's new apply changed nothing they could see. Any
    /// change to the spec answers the failure and starts a new rollout.
    #[test]
    fn scale_change_clears_rollout_failed() {
        let (mut inner, app_id) = failed_rollout("web:v2");
        let mut scaled = web_spec("web:v2");
        scaled.replicas = crate::config::Replicas::Fixed(5);
        inner.apply_request(&RaftRequest::AppSpec {
            app_id: app_id.clone(),
            spec: scaled,
        });
        assert_eq!(inner.state.rollouts[&app_id].failed, None);
    }

    /// #929: once a scale change has cleared the mark, the same version
    /// can fail again, and is marked again.
    #[test]
    fn a_version_that_fails_again_after_a_scale_change_is_marked_again() {
        let (mut inner, app_id) = failed_rollout("web:v2");
        let mut scaled = web_spec("web:v2");
        scaled.replicas = crate::config::Replicas::Fixed(5);
        inner.apply_request(&RaftRequest::AppSpec {
            app_id: app_id.clone(),
            spec: scaled.clone(),
        });
        let again = crate::meat::deploy_types::RolloutFailure {
            at_unix_ms: 2,
            ..failure_of(&scaled)
        };
        assert_eq!(
            inner.apply_request(&RaftRequest::RolloutFailed {
                app_id: app_id.clone(),
                failure: again.clone(),
            }),
            None
        );
        assert_eq!(inner.state.rollouts[&app_id].failed, Some(again));
    }

    #[test]
    fn new_spec_clears_rollout_failed() {
        let (mut inner, app_id) = failed_rollout("web:v2");
        inner.apply_request(&RaftRequest::AppSpec {
            app_id: app_id.clone(),
            spec: web_spec("web:v3"),
        });
        assert_eq!(inner.state.rollouts[&app_id].failed, None);
    }

    #[test]
    fn a_failure_of_a_spec_the_operator_replaced_is_refused() {
        let (mut inner, app_id) = failed_rollout("web:v2");
        inner.apply_request(&RaftRequest::AppSpec {
            app_id: app_id.clone(),
            spec: web_spec("web:v3"),
        });
        // A late report from another node of v2's rollout.
        let late = inner.apply_request(&RaftRequest::RolloutFailed {
            app_id: app_id.clone(),
            failure: failure_of(&web_spec("web:v2")),
        });
        assert!(matches!(late, Some(CouncilResponse::Refused { .. })));
        assert_eq!(inner.state.rollouts[&app_id].failed, None);
    }

    /// A node that redeploys a version already running healthy everywhere
    /// (its share of replicas moved while the cluster was full) and has to
    /// roll back hasn't found a bad version. Marking it would hold the app
    /// on "the previous spec", which is the same version, until the
    /// operator applied a different one: the 0.2.1 jobs soak's frontend
    /// stayed `rollout-failed` that way after an upgrade walk.
    #[test]
    fn a_failure_of_the_released_spec_is_refused() {
        let mut inner = StateMachineInner::default();
        let app_id = AppId::new("web", "default");
        let spec = web_spec("web:v1");
        inner.apply_request(&RaftRequest::AppSpec {
            app_id: app_id.clone(),
            spec: spec.clone(),
        });
        inner.apply_request(&RaftRequest::RolloutReleased {
            app_id: app_id.clone(),
            spec: spec.clone(),
        });
        let report = inner.apply_request(&RaftRequest::RolloutFailed {
            app_id: app_id.clone(),
            failure: failure_of(&spec),
        });
        assert!(
            matches!(&report, Some(CouncilResponse::Refused { reason }) if reason.contains("already rolled out")),
            "{report:?}"
        );
        assert_eq!(inner.state.rollouts[&app_id].failed, None);
    }

    /// The released version is v1; a failure of the new v2 is still marked.
    #[test]
    fn a_failure_of_a_new_version_after_a_release_is_marked() {
        let mut inner = StateMachineInner::default();
        let app_id = AppId::new("web", "default");
        inner.apply_request(&RaftRequest::AppSpec {
            app_id: app_id.clone(),
            spec: web_spec("web:v1"),
        });
        inner.apply_request(&RaftRequest::RolloutReleased {
            app_id: app_id.clone(),
            spec: web_spec("web:v1"),
        });
        inner.apply_request(&RaftRequest::AppSpec {
            app_id: app_id.clone(),
            spec: web_spec("web:v2"),
        });
        assert_eq!(
            inner.apply_request(&RaftRequest::RolloutFailed {
                app_id: app_id.clone(),
                failure: failure_of(&web_spec("web:v2")),
            }),
            None
        );
        assert_eq!(
            inner.state.rollouts[&app_id].failed,
            Some(failure_of(&web_spec("web:v2")))
        );
    }

    #[test]
    fn rollback_targets_are_capped_at_ten_per_app() {
        let mut inner = StateMachineInner::default();
        let app_id = AppId::new("web", "default");
        for version in 0..15 {
            let spec = web_spec(&format!("web:v{version}"));
            inner.apply_request(&RaftRequest::AppSpec {
                app_id: app_id.clone(),
                spec: spec.clone(),
            });
            assert_eq!(
                inner.apply_request(&RaftRequest::RolloutReleased {
                    app_id: app_id.clone(),
                    spec,
                }),
                None
            );
        }
        // v14 again, released a second time, doesn't take two slots.
        inner.apply_request(&RaftRequest::RolloutReleased {
            app_id: app_id.clone(),
            spec: web_spec("web:v14"),
        });
        let images: Vec<String> = inner.state.rollouts[&app_id]
            .released
            .iter()
            .filter_map(|spec| spec.image.clone())
            .collect();
        assert_eq!(
            images,
            (5..15).map(|v| format!("web:v{v}")).collect::<Vec<_>>()
        );
        // Releasing a spec that is no longer current is refused.
        assert!(matches!(
            inner.apply_request(&RaftRequest::RolloutReleased {
                app_id: app_id.clone(),
                spec: web_spec("web:v3"),
            }),
            Some(CouncilResponse::Refused { .. })
        ));
        // A deleted app takes its rollout record with it.
        inner.apply_request(&RaftRequest::AppDelete {
            app_id: app_id.clone(),
        });
        assert!(!inner.state.rollouts.contains_key(&app_id));
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
        let mut state = DesiredState::default();
        state.enter_recovery_epoch(0);
        CouncilStateMachine::persist_recovered_snapshot(&db, state).unwrap();
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
            ..autoscaled_spec(1, 10)
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
            ..autoscaled_spec(1, 10)
        };
        let spec4 = AppSpec {
            replicas: crate::config::types::Replicas::Fixed(4),
            ..autoscaled_spec(1, 10)
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
            ..autoscaled_spec(1, 10)
        };
        let spec_new_image = AppSpec {
            replicas: crate::config::types::Replicas::Fixed(2),
            image: Some("web:v2".to_string()),
            ..autoscaled_spec(1, 10)
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

    /// An app with two fixed replicas that autoscales between `min` and `max`.
    fn autoscaled_spec(min: u32, max: u32) -> AppSpec {
        toml::from_str(&format!(
            "image = \"test:v1\"\nreplicas = 2\n\
             [autoscale]\nmetric = \"cpu\"\ntarget = \"70%\"\nmin = {min}\nmax = {max}\n"
        ))
        .unwrap()
    }

    /// Apply `spec`, then an override to `replicas`, then each of `then` in
    /// turn, and return the override the app is left with.
    async fn override_after(
        spec: AppSpec,
        replicas: u32,
        then: Vec<AppSpec>,
    ) -> (Vec<CouncilResponse>, Vec<(String, u32)>) {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "prod");
        let mut entries = vec![
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
                    replicas,
                    reason: "load".to_string(),
                },
            ),
        ];
        for (offset, next) in then.into_iter().enumerate() {
            entries.push(normal_entry(
                1,
                3 + offset as u64,
                RaftRequest::AppSpec {
                    app_id: app_id.clone(),
                    spec: Box::new(next),
                },
            ));
        }
        let responses = sm.apply_fixture(entries).await.unwrap();
        (responses, sm.desired_state().await.autoscale_overrides)
    }

    #[tokio::test]
    async fn removing_autoscale_clears_the_override() {
        // #701: the autoscaler skips an app without `[autoscale]`, but the
        // scheduler kept applying its last override, pinning it at 8.
        let without = AppSpec {
            replicas: crate::config::types::Replicas::Fixed(2),
            ..default_spec()
        };
        let (_, overrides) = override_after(autoscaled_spec(1, 10), 8, vec![without]).await;
        assert!(
            overrides.is_empty(),
            "removing [autoscale] must clear the override: {overrides:?}"
        );
    }

    #[tokio::test]
    async fn narrowing_autoscale_bounds_clamps_the_override() {
        let (_, overrides) =
            override_after(autoscaled_spec(1, 10), 8, vec![autoscaled_spec(1, 5)]).await;
        assert_eq!(overrides, vec![("prod/web".to_string(), 5)]);

        let (_, overrides) =
            override_after(autoscaled_spec(1, 10), 2, vec![autoscaled_spec(4, 10)]).await;
        assert_eq!(overrides, vec![("prod/web".to_string(), 4)]);
    }

    #[tokio::test]
    async fn an_override_for_an_app_without_autoscale_is_refused() {
        // The autoscaler read a spec with `[autoscale]`; a deploy removing
        // it committed first. The late override must not pin the app.
        let (responses, overrides) = override_after(default_spec(), 8, Vec::new()).await;
        assert!(
            matches!(responses[1], CouncilResponse::Refused { .. }),
            "{responses:?}"
        );
        assert!(overrides.is_empty(), "{overrides:?}");
    }

    #[tokio::test]
    async fn an_override_outside_the_bounds_is_clamped() {
        let (_, overrides) = override_after(autoscaled_spec(1, 5), 9, Vec::new()).await;
        assert_eq!(overrides, vec![("prod/web".to_string(), 5)]);
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
                    deleted_at_unix_ms: 1,
                }),
            )])
            .await
            .unwrap();
        let state = restored.desired_state().await;
        // Deleting the tag untags only the copy; expiry removes it later.
        assert_eq!(state.manifest_catalog.manifests.len(), 2);
        assert!(
            state
                .manifest_catalog
                .get_manifest_by_tag("team-copy/app", "latest")
                .is_none()
        );
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
    async fn apply_delete_tag_untags_the_manifest() {
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
            deleted_at_unix_ms: 42,
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
        let digest = test_manifest_commit().manifest.digest;
        assert_eq!(
            state.manifest_catalog.untagged_since("myapp", &digest),
            Some(42)
        );
    }

    /// #697: the leader's ExpireManifest removes an untagged manifest whose
    /// untagging it observed, and nothing else.
    #[test]
    fn expire_manifest_removes_an_untagged_unpinned_manifest() {
        let mut inner = StateMachineInner::default();
        let commit = test_manifest_commit();
        let digest = commit.manifest.digest.clone();
        inner.apply_request(&RaftRequest::ManifestCommit(commit));
        inner.apply_request(&RaftRequest::DeleteTag(crate::pickle::types::DeleteTag {
            repository: "myapp".into(),
            tag: "latest".into(),
            deleted_at_unix_ms: 42,
        }));

        let response = inner.apply_request(&RaftRequest::ExpireManifest(
            crate::pickle::types::ExpireManifest {
                repository: "myapp".into(),
                digest: digest.clone(),
                untagged_at_unix_ms: 42,
            },
        ));

        assert!(response.is_none(), "{response:?}");
        assert!(
            inner
                .state
                .manifest_catalog
                .get_repository_manifest("myapp", digest.as_str())
                .is_none()
        );
    }

    /// #697: an app in desired state that pins the digest keeps the
    /// manifest, however long it has been untagged.
    #[test]
    fn expire_manifest_refuses_a_digest_an_app_pins() {
        let mut inner = StateMachineInner::default();
        let commit = test_manifest_commit();
        let digest = commit.manifest.digest.clone();
        inner.apply_request(&RaftRequest::ManifestCommit(commit));
        inner.apply_request(&RaftRequest::DeleteTag(crate::pickle::types::DeleteTag {
            repository: "myapp".into(),
            tag: "latest".into(),
            deleted_at_unix_ms: 42,
        }));
        inner.state.apps.insert(
            AppId::new("web", "prod"),
            AppSpec {
                image: Some(format!("myapp:latest@{}", digest.as_str())),
                ..default_spec()
            },
        );

        let response = inner.apply_request(&RaftRequest::ExpireManifest(
            crate::pickle::types::ExpireManifest {
                repository: "myapp".into(),
                digest: digest.clone(),
                untagged_at_unix_ms: 42,
            },
        ));

        assert!(
            matches!(response, Some(CouncilResponse::Refused { .. })),
            "{response:?}"
        );
        assert!(
            inner
                .state
                .manifest_catalog
                .get_repository_manifest("myapp", digest.as_str())
                .is_some()
        );
    }

    /// #692: a rollback target keeps its image, even once the app has moved
    /// on to another version: `relish rollback` must still be able to pull it.
    #[test]
    fn expire_manifest_refuses_a_digest_a_rollback_target_pins() {
        let mut inner = StateMachineInner::default();
        let commit = test_manifest_commit();
        let digest = commit.manifest.digest.clone();
        inner.apply_request(&RaftRequest::ManifestCommit(commit));
        inner.apply_request(&RaftRequest::DeleteTag(crate::pickle::types::DeleteTag {
            repository: "myapp".into(),
            tag: "latest".into(),
            deleted_at_unix_ms: 42,
        }));
        let app_id = AppId::new("web", "prod");
        let old = AppSpec {
            image: Some(format!("myapp:latest@{}", digest.as_str())),
            ..default_spec()
        };
        inner.apply_request(&RaftRequest::AppSpec {
            app_id: app_id.clone(),
            spec: Box::new(old.clone()),
        });
        inner.apply_request(&RaftRequest::RolloutReleased {
            app_id: app_id.clone(),
            spec: Box::new(old),
        });
        // The app now runs something else; only the rollback target names
        // the old digest.
        inner.apply_request(&RaftRequest::AppSpec {
            app_id,
            spec: Box::new(default_spec()),
        });

        let response = inner.apply_request(&RaftRequest::ExpireManifest(
            crate::pickle::types::ExpireManifest {
                repository: "myapp".into(),
                digest: digest.clone(),
                untagged_at_unix_ms: 42,
            },
        ));

        assert!(
            matches!(response, Some(CouncilResponse::Refused { .. })),
            "{response:?}"
        );
        assert!(
            inner
                .state
                .manifest_catalog
                .get_repository_manifest("myapp", digest.as_str())
                .is_some()
        );
    }

    /// #697: a decommissioned node's copies never serve again, so it leaves
    /// every holder set and heal sees the image as under-replicated.
    #[test]
    fn decommission_prunes_the_node_from_registry_holder_sets() {
        let mut inner = StateMachineInner::default();
        let node = "gone-for-good";
        let mut commit = test_manifest_commit();
        commit.holder_nodes = std::collections::BTreeSet::from([
            1,
            crate::cluster::identity::raft_id_from_name(node),
        ]);
        let digests: Vec<_> = commit
            .manifest
            .referenced_digests()
            .into_iter()
            .cloned()
            .collect();
        inner.apply_request(&RaftRequest::ManifestCommit(commit));

        assert!(matches!(
            inner.apply_request(&RaftRequest::DecommissionNode {
                node_id: node.into(),
                retired_by: "operator".into(),
                reason: "disk died".into(),
                retired_at_unix_ms: 20,
                membership_log_id: None,
            }),
            Some(CouncilResponse::NodeDecommissioned { .. })
        ));

        for digest in digests {
            assert_eq!(
                inner.state.manifest_catalog.layer_holders(digest.as_str()),
                std::collections::BTreeSet::from([1])
            );
        }
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
                role: crate::sesame::types::ApiRole::Admin,
                scope: crate::sesame::types::TokenScope::default(),
                expires_at: None,
                created_at: std::time::SystemTime::UNIX_EPOCH,
                previous_secret: None,
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

    fn short_of_room(placed: u32) -> crate::meat::types::Unschedulable {
        crate::meat::types::Unschedulable {
            placed,
            wanted: 3,
            reason: crate::meat::types::UnplacedReason::NoRoom,
        }
    }

    /// #702: the leader replaces the set of short apps, which survives a
    /// snapshot, keeps no ghost and goes with its app.
    #[test]
    fn unschedulable_replaces_the_set_and_goes_with_its_app() {
        let mut inner = StateMachineInner::default();
        apply_app(&mut inner, "web", "prod");
        apply_app(&mut inner, "api", "prod");
        let web = crate::meat::types::AppId::new("web", "prod");
        let api = crate::meat::types::AppId::new("api", "prod");
        let ghost = crate::meat::types::AppId::new("ghost", "prod");

        inner.apply_request(&RaftRequest::Unschedulable {
            apps: vec![
                (web.clone(), short_of_room(2)),
                (ghost.clone(), short_of_room(1)),
            ],
        });
        assert_eq!(inner.state.unschedulable.get(&web), Some(&short_of_room(2)));
        assert!(!inner.state.unschedulable.contains_key(&ghost));

        let json = serde_json::to_vec(&inner.state).unwrap();
        let restored: DesiredState = serde_json::from_slice(&json).unwrap();
        assert_eq!(restored.unschedulable.get(&web), Some(&short_of_room(2)));

        inner.apply_request(&RaftRequest::Unschedulable {
            apps: vec![(api.clone(), short_of_room(0))],
        });
        assert!(!inner.state.unschedulable.contains_key(&web));

        inner.apply_request(&RaftRequest::AppDelete { app_id: api });
        assert!(inner.state.unschedulable.is_empty());
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
            role: crate::sesame::types::ApiRole::Deployer,
            scope: crate::sesame::types::TokenScope::default(),
            expires_at: None,
            created_at: std::time::SystemTime::now(),
            previous_secret: None,
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
            role: crate::sesame::types::ApiRole::Deployer,
            scope: crate::sesame::types::TokenScope::default(),
            expires_at: None,
            created_at: std::time::SystemTime::now(),
            previous_secret: None,
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
            role,
            scope: crate::sesame::types::TokenScope::default(),
            expires_at: expires_unix_ms
                .map(|ms| std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms)),
            created_at: std::time::UNIX_EPOCH,
            previous_secret: None,
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
            role,
            scope: TokenScope::default(),
            expires_at: None,
            created_at: std::time::SystemTime::now(),
            previous_secret: None,
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

    fn rotation_for(name: &str, hash: u8) -> crate::sesame::types::TokenRotation {
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        crate::sesame::types::TokenRotation {
            name: name.to_string(),
            token_hash: vec![hash; 3],
            rotated_at: now,
            expires_at: None,
            previous_valid_until: Some(now + std::time::Duration::from_secs(3_600)),
        }
    }

    #[test]
    fn rotate_replaces_the_secret_and_keeps_the_name_role_and_scope() {
        use crate::sesame::types::{ApiRole, ApiToken, TokenScope};
        let scope = TokenScope {
            apps: Some(vec!["web".into()]),
            namespaces: None,
        };
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::CreateApiToken(ApiToken {
            name: "ci".to_string(),
            token_hash: vec![1, 2, 3],
            role: ApiRole::Deployer,
            scope: scope.clone(),
            expires_at: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            previous_secret: None,
        }));

        let response = inner.apply_request(&RaftRequest::RotateApiToken(rotation_for("ci", 9)));
        assert!(
            !matches!(response, Some(CouncilResponse::Refused { .. })),
            "{response:?}"
        );
        let token = &inner.state.security_state.api_tokens[0];
        assert_eq!(token.name, "ci");
        assert_eq!(token.role, ApiRole::Deployer);
        assert_eq!(token.scope, scope);
        assert_eq!(token.token_hash, vec![9; 3]);
        let previous = token.previous_secret.as_ref().unwrap();
        assert_eq!(previous.token_hash, vec![1, 2, 3]);
    }

    #[test]
    fn rotate_is_allowed_for_the_last_admin() {
        use crate::sesame::types::{ApiRole, ApiToken, TokenScope};
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::CreateApiToken(ApiToken {
            name: "root".to_string(),
            token_hash: vec![1, 2, 3],
            role: ApiRole::Admin,
            scope: TokenScope::default(),
            expires_at: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            previous_secret: None,
        }));
        let response = inner.apply_request(&RaftRequest::RotateApiToken(rotation_for("root", 7)));
        assert!(
            !matches!(response, Some(CouncilResponse::Refused { .. })),
            "{response:?}"
        );
        let tokens = &inner.state.security_state.api_tokens;
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].role, ApiRole::Admin);
        assert_eq!(tokens[0].token_hash, vec![7; 3]);
    }

    #[test]
    fn rotate_refuses_an_unknown_name_and_a_leased_test_token() {
        let mut inner = StateMachineInner::default();
        let response = inner.apply_request(&RaftRequest::RotateApiToken(rotation_for("ghost", 1)));
        assert!(matches!(response, Some(CouncilResponse::Refused { .. })));

        inner.state.security_state.api_tokens.push(leased_token());
        let name = inner.state.security_state.api_tokens[0].name.clone();
        let before = inner.state.security_state.api_tokens[0].clone();
        let response = inner.apply_request(&RaftRequest::RotateApiToken(rotation_for(&name, 1)));
        assert!(matches!(response, Some(CouncilResponse::Refused { .. })));
        assert_eq!(inner.state.security_state.api_tokens[0], before);
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
            binaries: Vec::new(),
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

    fn os_rollout(id: &str) -> crate::os::rollout::OsRollout {
        crate::os::rollout::plan(
            id,
            "2026.42.0",
            "https://example/os-channel.json",
            "n1",
            vec![(
                "n1".into(),
                "10.0.0.1:9117".into(),
                true,
                "2026.41.0".into(),
            )],
            0,
        )
    }

    #[test]
    fn an_os_rollout_and_a_bun_upgrade_never_overlap() {
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::OsRolloutUpdate {
            rollout: Box::new(os_rollout("os-1")),
        });
        inner.apply_request(&RaftRequest::UpgradeUpdate {
            state: Box::new(upgrade_state("up-1")),
        });
        assert!(
            inner.state.active_upgrade.is_none(),
            "no bun upgrade during an OS rollout"
        );
        inner.apply_request(&RaftRequest::OsRolloutUpdate {
            rollout: Box::new(os_rollout("os-2")),
        });
        assert_eq!(inner.state.os_rollout.as_ref().unwrap().rollout_id, "os-1");

        inner.apply_request(&RaftRequest::OsRolloutClear {
            rollout_id: "os-1".into(),
        });
        assert!(inner.state.os_rollout.is_none());
        assert_eq!(inner.state.os_rollout_history.len(), 1);

        inner.apply_request(&RaftRequest::UpgradeUpdate {
            state: Box::new(upgrade_state("up-1")),
        });
        inner.apply_request(&RaftRequest::OsRolloutUpdate {
            rollout: Box::new(os_rollout("os-3")),
        });
        assert!(
            inner.state.os_rollout.is_none(),
            "no OS rollout during a bun upgrade"
        );
    }

    #[test]
    fn a_paused_os_rollout_can_be_resumed_under_a_new_id() {
        let mut inner = StateMachineInner::default();
        let mut paused = os_rollout("os-1");
        paused.phase = crate::os::rollout::OsRolloutPhase::Paused {
            reason: "n1 fell back".into(),
        };
        inner.apply_request(&RaftRequest::OsRolloutUpdate {
            rollout: Box::new(paused.clone()),
        });
        inner.apply_request(&RaftRequest::OsRolloutUpdate {
            rollout: Box::new(crate::os::rollout::resume(paused, 10)),
        });
        assert_eq!(
            inner.state.os_rollout.as_ref().unwrap().rollout_id,
            "os-1-retry"
        );
    }

    #[test]
    fn the_council_size_is_cluster_state_that_survives_a_snapshot() {
        let mut inner = StateMachineInner::default();
        assert_eq!(inner.state.council_size, None);
        inner.apply_request(&RaftRequest::CouncilSize {
            voters: crate::council::CouncilSize::APPLIANCE,
        });
        assert_eq!(
            inner.state.council_size,
            Some(crate::council::CouncilSize::APPLIANCE)
        );
        let json = serde_json::to_vec(&inner.state).unwrap();
        let restored: DesiredState = serde_json::from_slice(&json).unwrap();
        assert_eq!(
            restored.council_size,
            Some(crate::council::CouncilSize::APPLIANCE)
        );
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

    fn build_record(name: &str) -> crate::bun::build_runner::BuildRecord {
        crate::bun::build_runner::BuildRecord {
            name: name.to_string(),
            runner_node: Some("n1".to_string()),
            state: crate::bun::build_runner::BuildState::Running,
            created_at_epoch_secs: 1_000_000,
            repository: None,
        }
    }

    fn task_array_register(count: u32) -> RaftRequest {
        RaftRequest::TaskArray(Box::new(
            crate::meat::task_array_store::TaskArrayWrite::Register {
                name: "render".to_string(),
                namespace: "default".to_string(),
                template: Box::new(crate::config::job::JobSpec {
                    runtime: crate::config::job::JobRuntime::Process,
                    image: None,
                    command: None,
                    schedule: None,
                    run_before: Vec::new(),
                    memory: None,
                    cpu: None,
                    env: Default::default(),
                    namespace: None,
                    exec: Some("/usr/bin/true".into()),
                    script: None,
                    firewall: None,
                    egress: None,
                    max_attempts: None,
                    task_timeout_secs: None,
                    overlap: None,
                    replay_unknown: false,
                }),
                spec: crate::meat::task_array::TaskArraySpec {
                    chunk_size: 4,
                    ..crate::meat::task_array::TaskArraySpec::with_count(count)
                },
                submitted_at_epoch_secs: 1_000_000,
            },
        ))
    }

    #[test]
    fn common_job_and_app_names_cannot_alias_in_the_same_namespace() {
        use crate::meat::{
            job::{JobDefinition, JobWrite},
            task_array_store::TaskArrayWrite,
        };
        let config = crate::config::Config::parse("[app.render]\nimage='web:v1'").unwrap();
        let app = RaftRequest::AppSpec {
            app_id: crate::meat::AppId::new("render", "default"),
            spec: Box::new(config.app["render"].clone()),
        };
        let job = RaftRequest::TaskArray(Box::new(TaskArrayWrite::Job(Box::new(JobWrite::Put {
            name: "render".into(),
            namespace: "default".into(),
            definition: Box::new(JobDefinition::from_spec(
                toml::from_str("runtime='process'\nexec='/bin/true'").unwrap(),
            )),
            trigger: None,
            now_epoch_secs: 1,
        }))));
        for (first, second) in [(&app, &job), (&job, &app)] {
            let mut inner = StateMachineInner::default();
            assert!(!matches!(
                inner.apply_request(first),
                Some(CouncilResponse::Refused { .. })
            ));
            let before = inner.state.clone();
            assert!(matches!(
                inner.apply_request(second),
                Some(CouncilResponse::Refused { .. })
            ));
            assert_eq!(
                serde_json::to_value(&inner.state).unwrap(),
                serde_json::to_value(&before).unwrap()
            );
        }
    }

    #[test]
    fn forgetting_a_definition_cannot_alias_its_retained_runs_as_an_app() {
        use crate::meat::{
            job::{JobDefinition, JobWrite, RunTrigger},
            task_array_store::TaskArrayWrite,
        };
        let mut inner = StateMachineInner::default();
        let job = RaftRequest::TaskArray(Box::new(TaskArrayWrite::Job(Box::new(JobWrite::Put {
            name: "reserved".into(),
            namespace: "default".into(),
            definition: Box::new(JobDefinition::from_spec(
                toml::from_str("runtime='process'\nexec='/bin/true'").unwrap(),
            )),
            trigger: Some(RunTrigger::Manual {
                request_id: "one".into(),
            }),
            now_epoch_secs: 1,
        }))));
        assert!(!matches!(
            inner.apply_request(&job),
            Some(CouncilResponse::Refused { .. })
        ));
        inner.apply_request(&RaftRequest::TaskArray(Box::new(
            TaskArrayWrite::StopDefinition {
                name: "reserved".into(),
                namespace: "default".into(),
                forget: true,
                now_epoch_secs: 2,
            },
        )));
        assert!(
            inner
                .state
                .task_arrays
                .jobs()
                .definition("default", "reserved")
                .is_none()
        );
        assert_eq!(inner.state.task_arrays.jobs().runs().count(), 1);
        let app = RaftRequest::AppSpec {
            app_id: crate::meat::AppId::new("reserved", "default"),
            spec: Box::new(
                crate::config::Config::parse("[app.reserved]\nimage='web:v1'")
                    .unwrap()
                    .app["reserved"]
                    .clone(),
            ),
        };
        let before = serde_json::to_value(&inner.state).unwrap();
        assert!(matches!(
            inner.apply_request(&app),
            Some(CouncilResponse::Refused { .. })
        ));
        assert_eq!(serde_json::to_value(&inner.state).unwrap(), before);
        inner.state.task_arrays.prune(4_000);
        assert_eq!(inner.state.task_arrays.jobs().runs().count(), 0);
        assert!(!matches!(
            inner.apply_request(&app),
            Some(CouncilResponse::Refused { .. })
        ));
    }

    #[tokio::test]
    async fn task_array_id_exhaustion_refuses_without_partial_registration() {
        use crate::meat::task_array_store::{ManifestCohort, TaskArrayWrite};
        for manifest in [false, true] {
            let mut sm = CouncilStateMachine::new();
            sm.inner.write().await.state.batch_ids.next_batch_id = u64::MAX - u64::from(manifest);
            let before = sm.desired_state().await.batch_ids.next_batch_id;
            let RaftRequest::TaskArray(array) = task_array_register(1) else {
                unreachable!()
            };
            let write = if manifest {
                let TaskArrayWrite::Register { template, spec, .. } = *array else {
                    unreachable!()
                };
                TaskArrayWrite::RegisterManifest {
                    name: "render".into(),
                    namespace: "default".into(),
                    request_id: "render-1".into(),
                    cohorts: vec![ManifestCohort {
                        name: "small".into(),
                        template: *template,
                        spec,
                    }],
                    submitted_at_epoch_secs: 1_000_000,
                }
            } else {
                *array
            };
            let responses = sm
                .apply(vec![normal_entry(
                    1,
                    1,
                    RaftRequest::TaskArray(Box::new(write)),
                )])
                .await
                .unwrap();
            assert!(matches!(responses[0], CouncilResponse::Refused { .. }));
            let state = sm.desired_state().await;
            assert_eq!(state.batch_ids.next_batch_id, before);
            assert!(state.task_arrays.ids().is_empty());
            assert_eq!(state.task_arrays.manifests().count(), 0);
        }
    }

    #[tokio::test]
    async fn common_hooks_gate_app_writes_across_a_leader_change() {
        use crate::meat::{
            index_set::IndexRangeSet, task_array::ChunkId, task_array_state::ChunkResult,
            task_array_store::TaskArrayWrite,
        };
        let mut sm = CouncilStateMachine::new();
        let op = "0123456789abcdef0123456789abcdef".to_string();
        let config = crate::config::Config::parse(
            "[app.web]\nimage='web:v1'\n[job.migrate]\nruntime='process'\nexec='/bin/true'\nrun_before=['app.web']",
        )
        .unwrap();
        sm.apply(vec![normal_entry(
            1,
            1,
            RaftRequest::TaskArray(Box::new(TaskArrayWrite::DeployBegin {
                operation_id: op.clone(),
                config: Box::new(config.clone()),
                now_epoch_secs: 10,
            })),
        )])
        .await
        .unwrap();
        let responses = sm
            .apply(vec![normal_entry(
                2,
                2,
                RaftRequest::JobApplyCommit {
                    operation_id: op.clone(),
                    now_epoch_secs: 11,
                },
            )])
            .await
            .unwrap();
        assert!(matches!(responses[0], CouncilResponse::Refused { .. }));
        assert!(sm.desired_state().await.apps.is_empty());
        let responses = sm
            .apply(vec![normal_entry(
                2,
                3,
                RaftRequest::AppSpec {
                    app_id: AppId::new("web", "default"),
                    spec: Box::new(config.app["web"].clone()),
                },
            )])
            .await
            .unwrap();
        assert!(matches!(responses[0], CouncilResponse::Refused { .. }));
        let worker = NodeId::new("worker");
        sm.apply(vec![
            normal_entry(
                2,
                4,
                RaftRequest::TaskArray(Box::new(TaskArrayWrite::Sync {
                    batch_id: 1,
                    now_epoch_secs: 11,
                    results: vec![],
                    grants: vec![(worker.clone(), IndexRangeSet::from_range(0..=0))],
                })),
            ),
            normal_entry(
                2,
                5,
                RaftRequest::TaskArray(Box::new(TaskArrayWrite::Sync {
                    batch_id: 1,
                    now_epoch_secs: 12,
                    grants: vec![],
                    results: vec![(
                        worker,
                        ChunkResult {
                            chunk: ChunkId(0),
                            attempt: 1,
                            succeeded: 1,
                            failed_count: 0,
                            failed_indices: IndexRangeSet::new(),
                            not_run: 0,
                            retried: 0,
                            retry_causes: Default::default(),
                            duration_counts: [0; 16],
                        },
                    )],
                })),
            ),
        ])
        .await
        .unwrap();
        let responses = sm
            .apply(vec![normal_entry(
                3,
                6,
                RaftRequest::JobApplyCommit {
                    operation_id: op,
                    now_epoch_secs: 13,
                },
            )])
            .await
            .unwrap();
        assert!(!matches!(responses[0], CouncilResponse::Refused { .. }));
        assert!(
            sm.desired_state()
                .await
                .apps
                .contains_key(&AppId::new("web", "default"))
        );
    }

    fn common_job_request(request_id: &str) -> RaftRequest {
        use crate::meat::job::{JobDefinition, JobWrite, RunTrigger};
        use crate::meat::task_array_store::TaskArrayWrite;
        let RaftRequest::TaskArray(array) = task_array_register(1) else {
            unreachable!()
        };
        let TaskArrayWrite::Register { template, spec, .. } = *array else {
            unreachable!()
        };
        RaftRequest::TaskArray(Box::new(TaskArrayWrite::Job(Box::new(JobWrite::Put {
            name: "render".into(),
            namespace: "default".into(),
            definition: Box::new(JobDefinition {
                template: *template,
                tasks: spec,
                cron: None,
                replay_unknown: false,
                hook: false,
            }),
            trigger: Some(RunTrigger::Manual {
                request_id: request_id.into(),
            }),
            now_epoch_secs: 1_000_000,
        }))))
    }

    #[tokio::test]
    async fn a_common_run_replay_after_term_change_needs_no_new_ids() {
        let mut sm = CouncilStateMachine::new();
        let responses = sm
            .apply(vec![normal_entry(1, 1, common_job_request("original"))])
            .await
            .unwrap();
        assert_eq!(
            responses[0],
            CouncilResponse::TaskArrayRegistered { batch_id: 1 }
        );
        sm.inner.write().await.state.batch_ids.next_batch_id = u64::MAX;
        let original = sm.desired_state().await.task_arrays;
        let responses = sm
            .apply(vec![normal_entry(2, 2, common_job_request("original"))])
            .await
            .unwrap();
        assert_eq!(
            responses[0],
            CouncilResponse::TaskArrayRegistered { batch_id: 1 }
        );
        let state = sm.desired_state().await;
        assert_eq!(state.task_arrays, original);
        assert_eq!(state.batch_ids.next_batch_id, u64::MAX);
    }

    #[tokio::test]
    async fn common_run_id_exhaustion_cannot_publish_a_definition_without_its_run() {
        let mut sm = CouncilStateMachine::new();
        sm.inner.write().await.state.batch_ids.next_batch_id = u64::MAX;
        let original = sm.desired_state().await.task_arrays;
        let responses = sm
            .apply(vec![normal_entry(1, 1, common_job_request("fresh"))])
            .await
            .unwrap();
        assert!(matches!(responses[0], CouncilResponse::Refused { .. }));
        let state = sm.desired_state().await;
        assert_eq!(state.task_arrays, original);
        assert!(
            state
                .task_arrays
                .jobs()
                .definition("default", "render")
                .is_none()
        );
        assert_eq!(state.batch_ids.next_batch_id, u64::MAX);
    }

    #[tokio::test]
    async fn task_arrays_take_consecutive_ids_from_the_shared_counter() {
        let mut sm = CouncilStateMachine::new();
        let responses = sm
            .apply_fixture(vec![
                normal_entry(1, 1, task_array_register(10)),
                normal_entry(1, 2, task_array_register(20)),
            ])
            .await
            .unwrap();
        assert_eq!(
            responses,
            vec![
                CouncilResponse::TaskArrayRegistered { batch_id: 1 },
                CouncilResponse::TaskArrayRegistered { batch_id: 2 },
            ]
        );
        let state = sm.desired_state().await;
        assert_eq!(state.task_arrays.get(2).unwrap().state.spec.count, 20);
        assert_eq!(state.batch_ids.next_batch_id, 3);
    }

    #[tokio::test]
    async fn refused_task_array_writes_leave_the_state_alone() {
        let mut sm = CouncilStateMachine::new();
        let responses = sm
            .apply(vec![
                normal_entry(1, 1, task_array_register(0)),
                normal_entry(
                    1,
                    2,
                    RaftRequest::TaskArray(Box::new(
                        crate::meat::task_array_store::TaskArrayWrite::Cancel {
                            now_epoch_secs: 0,
                            batch_id: 9,
                        },
                    )),
                ),
            ])
            .await
            .unwrap();
        assert!(matches!(responses[0], CouncilResponse::Refused { .. }));
        assert!(matches!(responses[1], CouncilResponse::Refused { .. }));
        let state = sm.desired_state().await;
        assert!(state.task_arrays.ids().is_empty());
        assert_eq!(state.batch_ids.next_batch_id, 1, "no id was burnt");
    }

    #[tokio::test]
    async fn task_array_progress_survives_a_snapshot() {
        use crate::meat::index_set::IndexRangeSet;
        use crate::meat::task_array_store::TaskArrayWrite;
        let mut sm = CouncilStateMachine::new();
        sm.apply(vec![
            normal_entry(1, 1, task_array_register(10)),
            normal_entry(
                1,
                2,
                RaftRequest::TaskArray(Box::new(TaskArrayWrite::Sync {
                    now_epoch_secs: 0,
                    batch_id: 1,
                    results: Vec::new(),
                    grants: vec![(
                        crate::meat::types::NodeId::new("n1"),
                        IndexRangeSet::from_range(0..=1),
                    )],
                })),
            ),
        ])
        .await
        .unwrap();
        let mut builder = sm.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.unwrap();
        let mut restored = CouncilStateMachine::new();
        restored
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        let state = restored.desired_state().await;
        let array = state.task_arrays.get(1).unwrap();
        assert_eq!(array.state.summary().held, 8);
        assert_eq!(state.batch_ids.next_batch_id, 2);
    }

    #[tokio::test]
    async fn a_tick_entry_reports_its_refused_arrays() {
        use crate::meat::index_set::IndexRangeSet;
        use crate::meat::task_array_store::TaskArrayWrite;
        let grant = |batch_id| TaskArrayWrite::Sync {
            now_epoch_secs: 0,
            batch_id,
            results: Vec::new(),
            grants: vec![(
                crate::meat::types::NodeId::new("n1"),
                IndexRangeSet::from_range(0..=0),
            )],
        };
        let mut sm = CouncilStateMachine::new();
        let responses = sm
            .apply_fixture(vec![
                normal_entry(1, 1, task_array_register(10)),
                normal_entry(
                    1,
                    2,
                    RaftRequest::TaskArray(Box::new(TaskArrayWrite::Tick {
                        writes: vec![grant(1), grant(9)],
                    })),
                ),
            ])
            .await
            .unwrap();
        let CouncilResponse::TaskArrayTicked { refused } = &responses[1] else {
            panic!("expected a tick, got {:?}", responses[1]);
        };
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].0, 9);
        let state = sm.desired_state().await;
        assert_eq!(state.task_arrays.get(1).unwrap().state.summary().held, 4);
    }

    /// A job lease in Raft and a job in its namespace, as a leased apply
    /// writes them: the lease, its ownership of the job, then the deployment.
    fn leased_job_entries() -> (String, String, Vec<RaftRequest>) {
        use crate::meat::task_array_store::TaskArrayWrite;
        let suffix = "0123456789abcdef0123456789abcdef";
        let lease = crate::testkit::lease::TestLease::new_scoped(
            format!("node-jobs-{suffix}"),
            "token:ci".into(),
            "ci".into(),
            format!("rbtest-node-{suffix}"),
            1_000,
            u64::MAX / 2,
            crate::testkit::lease::LeaseScope::NodeJobs,
        )
        .unwrap();
        let namespace = lease.namespace.clone();
        let lease_id = lease.lease_id.clone();
        let config = crate::config::Config::parse(&format!(
            "[job.migrate]\nimage = 'migration:v1'\nnamespace = '{namespace}'\ncpu = '4'\n"
        ))
        .unwrap();
        let entries = vec![
            RaftRequest::TestLeaseCreate(lease),
            RaftRequest::TestLeaseJobs {
                lease_id: lease_id.clone(),
                observed_at_unix_ms: 5_000,
                jobs: vec![crate::meat::AppId::new("migrate", &namespace)],
            },
            RaftRequest::TaskArray(Box::new(TaskArrayWrite::DeployBegin {
                operation_id: "c".repeat(32),
                config: Box::new(config),
                now_epoch_secs: 10,
            })),
        ];
        (lease_id, namespace, entries)
    }

    /// Leased applies now take the common job lifecycle (`JobApplyCommit`),
    /// so there is no claim tied to the leadership term that admitted them.
    /// A new leader reads the same lease and deployment, and nothing holds
    /// node capacity for a job no node has been granted yet.
    #[tokio::test]
    async fn leased_job_apply_survives_leader_change_without_reserving_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("leased.redb");
        let (lease_id, namespace, entries) = leased_job_entries();
        {
            let db = std::sync::Arc::new(Database::create(&path).unwrap());
            let mut sm = CouncilStateMachine::with_store(db).unwrap();
            let responses = sm
                .apply_fixture(
                    entries
                        .into_iter()
                        .enumerate()
                        .map(|(index, request)| normal_entry(1, index as u64 + 1, request))
                        .collect::<Vec<_>>(),
                )
                .await
                .unwrap();
            assert!(
                responses
                    .iter()
                    .all(|response| !matches!(response, CouncilResponse::Refused { .. })),
                "{responses:?}"
            );
            let mut builder = sm.get_snapshot_builder().await;
            builder.build_snapshot().await.unwrap();
        }

        // A later term reads the snapshot, as a new leader would.
        let db = std::sync::Arc::new(Database::create(&path).unwrap());
        let sm = CouncilStateMachine::with_store(db).unwrap();
        let desired = sm.desired_state().await;
        assert!(desired.test_leases[&lease_id].resources.contains(
            &crate::testkit::lease::LeasedResource::Job {
                job_id: crate::meat::AppId::new("migrate", &namespace),
            }
        ));
        assert!(
            desired
                .task_arrays
                .deployment_owner("migrate", &namespace)
                .is_some()
        );

        let mut cache = crate::meat::ClusterStateCache::new();
        cache.set_node(crate::meat::SchedulerNodeState {
            node_id: crate::meat::NodeId::new("worker"),
            allocatable: crate::meat::Resources::new(8000, 8 << 30, 0),
            allocated: crate::meat::Resources::default(),
            labels: Default::default(),
            ready: true,
            capabilities: crate::meat::cluster_state::NodeCapabilities::container_runtime(),
            app_replicas: Default::default(),
            uptime_secs: 0,
            cached_images: Default::default(),
        });
        crate::meat::admission::reserve_commitments(&mut cache, &desired);
        assert_eq!(
            cache
                .get_node(&crate::meat::NodeId::new("worker"))
                .unwrap()
                .allocated,
            crate::meat::Resources::default(),
            "an unscheduled leased job reserves nothing"
        );
    }

    /// A job lands in a test lease namespace only when an active job lease
    /// owns it, so the lease's cleanup can always find and stop it.
    #[tokio::test]
    async fn a_job_in_a_lease_namespace_needs_the_lease_to_own_it() {
        let (_, _, entries) = leased_job_entries();
        let mut sm = CouncilStateMachine::new();
        let without_ownership: Vec<_> = entries
            .into_iter()
            .filter(|request| !matches!(request, RaftRequest::TestLeaseJobs { .. }))
            .enumerate()
            .map(|(index, request)| normal_entry(1, index as u64 + 1, request))
            .collect();
        let responses = sm.apply_fixture(without_ownership).await.unwrap();
        assert!(matches!(responses[1], CouncilResponse::Refused { .. }));
        assert_eq!(
            sm.desired_state().await.task_arrays.deployments().count(),
            0
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
                normal_entry(1, 1, task_array_register(10)),
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
                normal_entry(2, 3, task_array_register(10)),
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
            CouncilResponse::TaskArrayRegistered { batch_id: 2 }
        );
        assert_eq!(
            responses[1],
            CouncilResponse::BuildRegistered { build_id: 2 }
        );

        // The pre-restart records reloaded too.
        let state = sm.desired_state().await;
        assert!(state.task_arrays.get(1).is_some());
        assert!(state.build_state.get(1).is_some());
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
            role: crate::sesame::types::ApiRole::Deployer,
            scope: crate::sesame::types::TokenScope {
                apps: None,
                namespaces: Some(vec!["rbtest-run1".into()]),
            },
            expires_at: Some(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(100),
            ),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            previous_secret: None,
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
        inner.apply_request(&RaftRequest::TestLeaseAppDelete {
            lease_id: "run1".into(),
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

    /// Job leases live in Raft like application leases, so every node and
    /// a later leader can check them. They still own only jobs.
    #[tokio::test]
    async fn job_leases_enter_replicated_state_and_own_only_jobs() {
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
        let lease_id = lease.lease_id.clone();
        let namespace = lease.namespace.clone();
        let responses = sm
            .apply_fixture(vec![
                normal_entry(1, 1, RaftRequest::TestLeaseCreate(lease)),
                normal_entry(
                    1,
                    2,
                    RaftRequest::TestLeaseAppSpec {
                        lease_id: lease_id.clone(),
                        observed_at_unix_ms: 20,
                        app_id: AppId::new("web", &namespace),
                        spec: Box::new(default_spec()),
                    },
                ),
            ])
            .await
            .unwrap();
        assert!(!matches!(responses[0], CouncilResponse::Refused { .. }));
        assert!(
            matches!(responses[1], CouncilResponse::Refused { .. }),
            "a job lease cannot own an app"
        );
        let state = sm.desired_state().await;
        assert!(state.test_leases[&lease_id].resources.is_empty());
        assert!(state.apps.is_empty());
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
        requests.push(RaftRequest::TestLeaseAppDelete {
            lease_id: "run1".into(),
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
            RaftRequest::TestLeaseAppDelete {
                lease_id: "run1".into(),
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
            RaftRequest::TestLeaseAppDelete {
                lease_id: "run1".into(),
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

    fn gitops_write(term: u64, change: RaftRequest) -> RaftRequest {
        RaftRequest::GitOpsWrite {
            term,
            change: Box::new(change),
        }
    }

    fn namespace_spec() -> Box<crate::config::NamespaceSpec> {
        Box::new(toml::from_str(r#"cpu = "1000m""#).unwrap())
    }

    /// #693: a GitOps write records the resource it wrote in the same
    /// entry, and a manual write of another resource is not recorded.
    #[tokio::test]
    async fn gitops_write_records_ownership() {
        let mut sm = CouncilStateMachine::new();
        let web = AppId::new("web", "prod");
        let manual = AppId::new("manual", "prod");
        let responses = sm
            .apply_fixture(vec![
                normal_entry(
                    3,
                    1,
                    gitops_write(
                        3,
                        RaftRequest::NamespaceSpec {
                            name: "prod".into(),
                            spec: namespace_spec(),
                        },
                    ),
                ),
                normal_entry(
                    3,
                    2,
                    gitops_write(
                        3,
                        RaftRequest::PermissionSpec {
                            name: "deployers".into(),
                            spec: Box::new(crate::config::PermissionSpec {
                                actions: vec!["deploy".into()],
                                apps: vec![],
                                namespaces: None,
                            }),
                        },
                    ),
                ),
                normal_entry(
                    3,
                    3,
                    gitops_write(
                        3,
                        RaftRequest::AppSpec {
                            app_id: web.clone(),
                            spec: Box::new(default_spec()),
                        },
                    ),
                ),
                normal_entry(
                    3,
                    4,
                    RaftRequest::AppSpec {
                        app_id: manual.clone(),
                        spec: Box::new(default_spec()),
                    },
                ),
            ])
            .await
            .unwrap();
        assert!(
            responses
                .iter()
                .all(|r| !matches!(r, CouncilResponse::Refused { .. })),
            "{responses:?}"
        );
        let state = sm.desired_state().await;
        assert!(state.apps.contains_key(&web));
        assert!(state.gitops_owned.apps.contains(&web));
        assert!(!state.gitops_owned.apps.contains(&manual));
        assert!(state.gitops_owned.namespaces.contains("prod"));
        assert!(state.gitops_owned.permissions.contains("deployers"));
    }

    /// #693: pruning a GitOps resource removes it from the ownership set,
    /// and GitOps may never delete a resource it didn't write.
    #[tokio::test]
    async fn prune_clears_ownership() {
        let mut sm = CouncilStateMachine::new();
        let web = AppId::new("web", "default");
        let manual = AppId::new("manual", "default");
        let responses = sm
            .apply_fixture(vec![
                normal_entry(
                    2,
                    1,
                    gitops_write(
                        2,
                        RaftRequest::AppSpec {
                            app_id: web.clone(),
                            spec: Box::new(default_spec()),
                        },
                    ),
                ),
                normal_entry(
                    2,
                    2,
                    RaftRequest::AppSpec {
                        app_id: manual.clone(),
                        spec: Box::new(default_spec()),
                    },
                ),
                normal_entry(
                    2,
                    3,
                    gitops_write(
                        2,
                        RaftRequest::AppDelete {
                            app_id: manual.clone(),
                        },
                    ),
                ),
                normal_entry(
                    2,
                    4,
                    gitops_write(
                        2,
                        RaftRequest::AppDelete {
                            app_id: web.clone(),
                        },
                    ),
                ),
            ])
            .await
            .unwrap();
        assert!(
            matches!(&responses[2], CouncilResponse::Refused { reason } if reason.contains("GitOps")),
            "{:?}",
            responses[2]
        );
        let state = sm.desired_state().await;
        assert!(state.apps.contains_key(&manual), "manual app survives");
        assert!(!state.apps.contains_key(&web));
        assert!(state.gitops_owned.apps.is_empty());
    }

    /// #693 (finding 7): a GitOps write proposed in a different term from
    /// the one its sync read its snapshot in is refused, so an A→B→A
    /// leadership flap can't apply a stale diff.
    #[tokio::test]
    async fn a_gitops_write_from_an_earlier_term_is_refused() {
        let mut sm = CouncilStateMachine::new();
        let web = AppId::new("web", "default");
        let responses = sm
            .apply_fixture(vec![normal_entry(
                5,
                1,
                gitops_write(
                    3,
                    RaftRequest::AppSpec {
                        app_id: web.clone(),
                        spec: Box::new(default_spec()),
                    },
                ),
            )])
            .await
            .unwrap();
        assert!(
            matches!(&responses[0], CouncilResponse::Refused { reason } if reason.contains("term")),
            "{responses:?}"
        );
        let state = sm.desired_state().await;
        assert!(state.apps.is_empty());
        assert!(state.gitops_owned.apps.is_empty());
    }

    /// Only declarative writes travel inside a GitOps write.
    #[tokio::test]
    async fn a_gitops_write_carries_only_declarative_changes() {
        let mut sm = CouncilStateMachine::new();
        let responses = sm
            .apply_fixture(vec![normal_entry(
                1,
                1,
                gitops_write(
                    1,
                    RaftRequest::ConfigSet {
                        key: "k".into(),
                        value: "v".into(),
                    },
                ),
            )])
            .await
            .unwrap();
        assert!(matches!(&responses[0], CouncilResponse::Refused { .. }));
    }

    /// A manual delete of a GitOps resource (`relish delete --force`)
    /// leaves the ownership set describing only resources that exist.
    #[tokio::test]
    async fn a_manual_delete_forgets_ownership() {
        let mut sm = CouncilStateMachine::new();
        let web = AppId::new("web", "default");
        sm.apply_fixture(vec![
            normal_entry(
                1,
                1,
                gitops_write(
                    1,
                    RaftRequest::AppSpec {
                        app_id: web.clone(),
                        spec: Box::new(default_spec()),
                    },
                ),
            ),
            normal_entry(
                1,
                2,
                RaftRequest::AppDelete {
                    app_id: web.clone(),
                },
            ),
        ])
        .await
        .unwrap();
        assert!(sm.desired_state().await.gitops_owned.apps.is_empty());
    }

    /// #693 (finding 2): a plain delete can't remove an app or namespace a
    /// live test lease owns; only a delete carrying that lease can.
    #[test]
    fn app_delete_in_a_leased_namespace_needs_the_lease() {
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::TestLeaseCreate(test_lease("run1", 100)));
        let app_id = AppId::new("web", "rbtest-run1");
        assert!(
            inner
                .apply_request(&RaftRequest::TestLeaseNamespaceSpec {
                    lease_id: "run1".into(),
                    observed_at_unix_ms: 20,
                    name: "rbtest-run1".into(),
                    spec: namespace_spec(),
                })
                .is_none()
        );
        assert!(
            inner
                .apply_request(&RaftRequest::TestLeaseAppSpec {
                    lease_id: "run1".into(),
                    observed_at_unix_ms: 20,
                    app_id: app_id.clone(),
                    spec: Box::new(default_spec()),
                })
                .is_none()
        );

        for refused in [
            RaftRequest::AppDelete {
                app_id: app_id.clone(),
            },
            RaftRequest::NamespaceDelete {
                name: "rbtest-run1".into(),
            },
            RaftRequest::TestLeaseAppDelete {
                lease_id: "other".into(),
                app_id: app_id.clone(),
            },
        ] {
            assert!(
                matches!(
                    inner.apply_request(&refused),
                    Some(CouncilResponse::Refused { .. })
                ),
                "{refused:?}"
            );
        }
        assert!(inner.state.apps.contains_key(&app_id));
        assert!(inner.state.namespaces.contains_key("rbtest-run1"));

        assert!(
            inner
                .apply_request(&RaftRequest::TestLeaseAppDelete {
                    lease_id: "run1".into(),
                    app_id: app_id.clone(),
                })
                .is_none()
        );
        assert!(
            inner
                .apply_request(&RaftRequest::TestLeaseNamespaceDelete {
                    lease_id: "run1".into(),
                    name: "rbtest-run1".into(),
                })
                .is_none()
        );
        assert!(!inner.state.apps.contains_key(&app_id));
        assert!(!inner.state.namespaces.contains_key("rbtest-run1"));
    }

    /// #693: a node's `[gitops]` seeds the cluster's configuration only
    /// when there is none; an explicit set replaces it, and clears it.
    #[test]
    fn a_seed_never_replaces_the_cluster_gitops_config() {
        let stored = |repo: &str| {
            Some(Box::new(crate::lettuce::types::StoredGitOpsConfig {
                config: toml::from_str(&format!("repo = '{repo}'")).unwrap(),
                sealed_credentials: None,
            }))
        };
        let mut inner = StateMachineInner::default();
        inner.apply_request(&RaftRequest::GitOpsConfigSet {
            config: stored("first"),
            seed: true,
        });
        inner.apply_request(&RaftRequest::GitOpsConfigSet {
            config: stored("second"),
            seed: true,
        });
        let repo = |inner: &StateMachineInner| {
            inner
                .state
                .gitops_config
                .as_ref()
                .map(|stored| stored.config.repo.clone())
        };
        assert_eq!(repo(&inner).as_deref(), Some("first"));
        inner.apply_request(&RaftRequest::GitOpsConfigSet {
            config: stored("third"),
            seed: false,
        });
        assert_eq!(repo(&inner).as_deref(), Some("third"));
        inner.apply_request(&RaftRequest::GitOpsConfigSet {
            config: None,
            seed: false,
        });
        assert_eq!(repo(&inner), None);
    }
}
