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
const SNAPSHOT_FORMAT_VERSION: u32 = 1;

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
    #[error(
        "snapshot format version {found} is not supported (this binary supports up to {supported})"
    )]
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

/// Write the latest snapshot (data + index + version + checksum) to redb,
/// fsyncing on commit. A legacy (pre-envelope) store is upgraded in place:
/// the version and checksum keys land in the same write transaction as the
/// payload they describe.
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

/// Read the stored format version, `None` for a legacy pre-envelope store.
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

impl StateMachineInner {
    /// Apply a request. Returns a request-specific response for entries
    /// that carry a verdict back to the proposer (`AllocateSerial` gets
    /// its serial, `GcReport` gets the approved deletions); `None` means
    /// the generic `Applied` response.
    fn apply_request(&mut self, request: &RaftRequest) -> Option<CouncilResponse> {
        match request {
            RaftRequest::AppSpec { app_id, spec } => {
                if crate::testkit::lease::valid_test_namespace(&app_id.namespace) {
                    return Some(CouncilResponse::Refused {
                        reason: "test lease namespace requires a leased app write".to_string(),
                    });
                }
                self.apply_app_spec(app_id, spec);
            }
            RaftRequest::AppDelete { app_id } => {
                self.state.apps.remove(app_id);
                self.state.scheduling.remove(app_id);
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
                self.state.security_state = *ss.clone();
            }
            RaftRequest::CreateJoinToken(jt) => {
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
                if token.consumed {
                    return Some(CouncilResponse::Refused {
                        reason: "join token already consumed".to_string(),
                    });
                }
                token.consumed = true;
                let serial = self.state.security_state.next_serial;
                self.state.security_state.next_serial += 1;
                return Some(CouncilResponse::JoinTokenConsumed { serial });
            }
            RaftRequest::CreateApiToken(token) => {
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
            RaftRequest::AllocateSerial => {
                // Return the pre-increment value as this entry's serial.
                let serial = self.state.security_state.next_serial;
                self.state.security_state.next_serial += 1;
                return Some(CouncilResponse::SerialAllocated { serial });
            }
            RaftRequest::RotateSecretKey { scope, new_keypair } => {
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
                // seal (legacy state) count as "unknown generation" and
                // block finalise until re-encrypted.
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
                self.state
                    .security_state
                    .age_keypairs
                    .retain(|kp| kp.scope != *scope || !kp.read_only);
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
            RaftRequest::BatchRegister { batch } => {
                // The durable counter is the id authority (JOB4): a
                // restarted leader continues from the replicated value
                // instead of reusing ids from 1. Registration also prunes
                // stale terminal batches, keyed on the *request's* clock
                // so every replica prunes identically.
                let batch_id = self.state.batch_state.register(batch.clone());
                return Some(CouncilResponse::BatchRegistered { batch_id });
            }
            RaftRequest::BatchJobUpdate {
                batch_id,
                job_name,
                status,
            } => {
                // Transition validation lives here, at the single point
                // every replica passes through: forged states, unknown
                // jobs and conflicting terminal reports are refused;
                // duplicate terminal reports apply as no-ops (JOB3).
                if let Err(e) = self.state.batch_state.report(*batch_id, job_name, *status) {
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
            RaftRequest::PublishEndpoints(catalog) => {
                // Wholesale replacement: the leader is the single source of
                // truth for the catalogue, so a later publish always wins.
                self.state.endpoint_catalog = *catalog.clone();
            }
            RaftRequest::TestLeaseCreate(lease) => {
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
            RaftRequest::TestLeaseAppSpec {
                lease_id,
                observed_at_unix_ms,
                app_id,
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
                if app_id.namespace != lease.namespace {
                    return Some(CouncilResponse::Refused {
                        reason: "app namespace does not match its lease".to_string(),
                    });
                }
                let resource = crate::testkit::lease::LeasedResource::App {
                    app_id: app_id.clone(),
                };
                let already_owned = lease.resources.contains(&resource);
                if !already_owned
                    && lease.resources.len() >= crate::testkit::lease::MAX_LEASED_RESOURCES
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
                    && lease.resources.len() >= crate::testkit::lease::MAX_LEASED_RESOURCES
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
    /// older than `newest` — including secrets with no recorded seal
    /// (legacy state written before seals existed). Sorted, as
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
        // Materialise the table so the read doesn't error on a fresh store.
        let wtx = db.begin_write()?;
        {
            wtx.open_table(SNAPSHOT)?;
        }
        wtx.commit()?;
        let rtx = db.begin_read()?;
        let t = rtx.open_table(SNAPSHOT)?;
        Ok(t.get(SNAP_DATA_KEY)?.is_some())
    }

    #[allow(clippy::result_large_err)]
    pub fn with_store(db: Arc<Database>) -> Result<Self, SnapshotStoreError> {
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
                match read_snapshot_version(&t)? {
                    // Legacy pre-envelope snapshot: no version, no checksum.
                    // Load it as before; the next persist rewrites it in the
                    // enveloped format (version + checksum, one transaction).
                    None => eprintln!(
                        "council: snapshot has no version/checksum envelope (pre-12b format); \
                         loading as legacy, it will be rewritten on the next snapshot"
                    ),
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
            }
            if let Some(idx) = t.get(SNAP_INDEX_KEY)?
                && let Ok(le) = <[u8; 8]>::try_from(idx.value())
            {
                inner.snapshot_index = u64::from_le_bytes(le);
            }
        }
        inner.db = Some(db);
        Ok(Self {
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
        state.recovery_epoch = state.recovery_epoch.saturating_add(1);
        // A recovered node re-bootstraps its own Raft: the old log id and
        // membership belong to the cluster that died, so we drop them and let
        // `initialize` establish a fresh term line and voter set.
        state.last_applied_log = None;
        state.last_membership = Default::default();

        let inner = StateMachineInner {
            state,
            ..StateMachineInner::default()
        };
        Self {
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
        state.recovery_epoch = state.recovery_epoch.saturating_add(1);
        state.last_applied_log = None;
        state.last_membership = StoredMembership::default();
        // Serialisation of a plain struct with only owned data is infallible in
        // practice; map any error into a redb error rather than panicking.
        let data = serde_json::to_vec(&state)
            .map_err(|e| redb::Error::Io(std::io::Error::other(e.to_string())))?;
        persist_snapshot(snapshot_db, &data, 1)
    }

    /// Read the current desired state.
    pub async fn desired_state(&self) -> DesiredState {
        self.inner.read().await.state.clone()
    }

    /// Log id the loaded snapshot covers up to, `None` when no snapshot is
    /// loaded. Only meaningful straight after `with_store`, before Raft
    /// applies new entries; `council::validate_purge_boundary` reads it at
    /// startup to check the snapshot still covers the purged log prefix.
    pub async fn snapshot_last_applied(&self) -> Option<LogId<u64>> {
        let guard = self.inner.read().await;
        if guard.snapshot_data.is_some() {
            guard.state.last_applied_log
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

        let (db, index) = {
            let mut guard = self.inner.write().await;
            guard.state = new_state;
            guard.state.last_applied_log = meta.last_log_id;
            guard.state.last_membership = meta.last_membership.clone();
            guard.snapshot_index += 1;
            guard.snapshot_data = Some(data.clone());
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
        let (data, index, db, last_log_id, last_membership) = {
            let mut guard = self.inner.write().await;
            let data = serde_json::to_vec(&guard.state)
                .map_err(|e| StorageError::from(StorageIOError::read_state_machine(&e)))?;
            guard.snapshot_index += 1;
            guard.snapshot_data = Some(data.clone());
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

    /// Persist a small known state to `path` via the real snapshot path,
    /// returning the app id it contains. Everything is dropped before
    /// returning so the caller can reopen (or tamper with) the store.
    async fn persist_known_snapshot(path: &std::path::Path) -> AppId {
        let app_id = AppId::new("web", "prod");
        let db = std::sync::Arc::new(Database::create(path).unwrap());
        let mut sm = CouncilStateMachine::with_store(db).unwrap();
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
    async fn legacy_snapshot_without_envelope_still_loads_and_is_rewritten() {
        // Fixture: a snapshot exactly as a pre-envelope binary wrote it —
        // raw `DesiredState` JSON under "data" plus the counter under
        // "index", no version or checksum keys. Existing dev clusters and
        // the Lima rigs carry this format; it must keep loading (with a
        // warning), and the next persist must upgrade it in place.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.redb");
        let app_id = AppId::new("web", "prod");
        let mut legacy_state = DesiredState::default();
        legacy_state.apps.insert(
            app_id.clone(),
            AppSpec {
                image: Some("legacy:v1".to_string()),
                ..default_spec()
            },
        );
        legacy_state.last_applied_log = Some(log_id(1, 3));
        let payload = serde_json::to_vec(&legacy_state).unwrap();
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
        let mut sm = CouncilStateMachine::with_store(db.clone()).expect("legacy snapshot loads");
        let state = sm.desired_state().await;
        assert_eq!(
            state.apps.get(&app_id).unwrap().image,
            Some("legacy:v1".to_string())
        );
        assert_eq!(sm.snapshot_last_applied().await, Some(log_id(1, 3)));

        // The next snapshot persist rewrites the store in the new format.
        let mut builder = sm.get_snapshot_builder().await;
        builder.build_snapshot().await.unwrap();
        let rtx = db.begin_read().unwrap();
        let t = rtx.open_table(SNAPSHOT).unwrap();
        assert!(t.get(SNAP_VERSION_KEY).unwrap().is_some());
        assert!(t.get(SNAP_CHECKSUM_KEY).unwrap().is_some());
    }

    #[tokio::test]
    async fn snapshot_last_applied_is_none_without_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(Database::create(dir.path().join("bare.redb")).unwrap());
        let sm = CouncilStateMachine::with_store(db).unwrap();
        assert_eq!(sm.snapshot_last_applied().await, None);
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
    async fn app_delete_clears_a_stale_autoscale_override() {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "prod");
        let spec = AppSpec {
            replicas: crate::config::types::Replicas::Fixed(2),
            ..default_spec()
        };
        sm.apply(vec![
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
        sm.apply(vec![
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
        sm.apply(vec![
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
                    node_id: "node-b".to_string(),
                    node_ip: std::net::Ipv4Addr::new(10, 0, 0, 2),
                    host_port: 30002,
                    healthy: true,
                }],
            ),
        ]);
        let entry = normal_entry(1, 1, RaftRequest::PublishEndpoints(Box::new(catalog)));
        sm.apply(vec![entry]).await.unwrap();

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
            EndpointCatalog::rebuild([(ServiceId::new("default", "web"), 80, vec![])]);
        let entry2 = normal_entry(2, 1, RaftRequest::PublishEndpoints(Box::new(replacement)));
        sm.apply(vec![entry2]).await.unwrap();
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
        let remaining = &inner.state.security_state.age_keypairs;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].generation, 1);
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
        });

        let response = inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: AgeKeyScope::ClusterWide,
            new_keypair: test_age_keypair(AgeKeyScope::ClusterWide, 2, false),
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
        });

        let response = inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: AgeKeyScope::ClusterWide,
            new_keypair: test_age_keypair(AgeKeyScope::ClusterWide, 1, false),
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

    /// PKI8 fixture: security state persisted before `secret_seals`
    /// existed loads fine (self-describing JSON, `#[serde(default)]`),
    /// and its secrets — with no recorded generation — block finalize
    /// until re-encrypted.
    #[test]
    fn legacy_secret_without_generation_metadata_blocks_finalize() {
        use crate::sesame::types::AgeKeyScope;

        // A legacy SecurityState JSON with no `secret_seals` field.
        let legacy_json = r#"{
            "certificate_authorities": [],
            "age_keypairs": [],
            "api_tokens": [],
            "join_tokens": [],
            "next_serial": 6
        }"#;
        let legacy: crate::sesame::types::SecurityState =
            serde_json::from_str(legacy_json).unwrap();
        assert!(legacy.secret_seals.is_empty(), "missing field defaults");

        let mut inner = StateMachineInner::default();
        inner.state.security_state = legacy;
        inner
            .state
            .security_state
            .age_keypairs
            .push(test_age_keypair(AgeKeyScope::ClusterWide, 0, false));
        // The app predates seal recording: insert it directly, the way a
        // pre-upgrade snapshot would restore it — no seal entry.
        inner.state.apps.insert(
            crate::meat::types::AppId::new("web", "default"),
            spec_with_encrypted_env(),
        );

        inner.apply_request(&RaftRequest::RotateSecretKey {
            scope: AgeKeyScope::ClusterWide,
            new_keypair: test_age_keypair(AgeKeyScope::ClusterWide, 1, false),
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

    // -- Batch/build durable tracking (12b.2) ---------------------------------

    fn batch_record(job: &str, node: &str) -> crate::meat::batch_tracker::BatchRecord {
        crate::meat::batch_tracker::BatchRecord {
            jobs: vec![crate::meat::batch_tracker::BatchJobRecord {
                name: job.to_string(),
                namespace: "default".to_string(),
                node: Some(crate::meat::types::NodeId::new(node)),
                status: crate::meat::batch_tracker::JobStatus::Pending,
            }],
            submitted_at_epoch_secs: 1_000_000,
        }
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
            .apply(vec![
                normal_entry(
                    1,
                    1,
                    RaftRequest::BatchRegister {
                        batch: batch_record("j1", "n1"),
                    },
                ),
                normal_entry(
                    1,
                    2,
                    RaftRequest::BatchRegister {
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
            sm.apply(vec![
                normal_entry(
                    1,
                    1,
                    RaftRequest::BatchRegister {
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
            .apply(vec![
                normal_entry(
                    2,
                    3,
                    RaftRequest::BatchRegister {
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
        sm.apply(vec![normal_entry(
            1,
            1,
            RaftRequest::BatchRegister {
                batch: batch_record("j1", "n1"),
            },
        )])
        .await
        .unwrap();

        // A legal completion applies…
        let ok = sm
            .apply(vec![normal_entry(
                1,
                2,
                RaftRequest::BatchJobUpdate {
                    batch_id: 1,
                    job_name: "j1".to_string(),
                    status: crate::meat::batch_tracker::JobStatus::Completed,
                },
            )])
            .await
            .unwrap();
        assert_eq!(ok[0], CouncilResponse::Applied { log_index: 2 });

        // …a conflicting terminal report is refused…
        let refused = sm
            .apply(vec![normal_entry(
                1,
                3,
                RaftRequest::BatchJobUpdate {
                    batch_id: 1,
                    job_name: "j1".to_string(),
                    status: crate::meat::batch_tracker::JobStatus::Failed,
                },
            )])
            .await
            .unwrap();
        assert!(matches!(refused[0], CouncilResponse::Refused { .. }));

        // …and so is a report for an unknown batch.
        let unknown = sm
            .apply(vec![normal_entry(
                1,
                4,
                RaftRequest::BatchJobUpdate {
                    batch_id: 99,
                    job_name: "j1".to_string(),
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
        sm.apply(vec![normal_entry(
            1,
            1,
            RaftRequest::BuildRegister {
                build: build_record("b1"),
            },
        )])
        .await
        .unwrap();

        let ok = sm
            .apply(vec![normal_entry(
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
            .apply(vec![
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
            .apply(vec![normal_entry(
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
    async fn leased_app_write_atomically_records_ownership() {
        let mut sm = CouncilStateMachine::new();
        let app_id = AppId::new("web", "rbtest-run1");
        let responses = sm
            .apply(vec![
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
            .apply(vec![
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
        sm.apply(vec![normal_entry(
            1,
            2,
            RaftRequest::TestLeaseCreate(test_lease("run1", 30)),
        )])
        .await
        .unwrap();
        let responses = sm
            .apply(vec![
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
            .apply(vec![
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
        let responses = sm.apply(entries).await.unwrap();
        assert!(
            responses
                .iter()
                .all(|response| matches!(response, CouncilResponse::Applied { .. }))
        );
        let responses = sm
            .apply(vec![normal_entry(
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
        sm.apply(vec![normal_entry(
            1,
            1,
            RaftRequest::TestLeaseCreate(test_lease("run1", 100)),
        )])
        .await
        .unwrap();
        sm.apply(vec![normal_entry(
            1,
            2,
            RaftRequest::TestLeaseBeginCleanup {
                lease_id: "run1".to_string(),
            },
        )])
        .await
        .unwrap();
        sm.apply(vec![normal_entry(
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
        sm.apply(vec![normal_entry(
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
        sm.apply(vec![normal_entry(
            1,
            1,
            RaftRequest::TestLeaseCreate(test_lease("run1", 100)),
        )])
        .await
        .unwrap();

        // Race: an app attaches to the lease after that snapshot but before
        // cleanup begins. The lease is still Active, so the write is accepted.
        let app_id = AppId::new("web", "rbtest-run1");
        sm.apply(vec![normal_entry(
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
        sm.apply(vec![normal_entry(
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
            .apply(vec![normal_entry(
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
        sm.apply(vec![normal_entry(
            1,
            5,
            RaftRequest::AppDelete {
                app_id: app_id.clone(),
            },
        )])
        .await
        .unwrap();
        let responses = sm
            .apply(vec![normal_entry(
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

    /// The 12b.2 compatibility rule made executable: a snapshot written
    /// before the batch/build tracker fields existed must load cleanly,
    /// with both trackers defaulting to empty and counters at 1.
    #[test]
    fn pre_12b2_snapshot_without_tracker_fields_still_loads() {
        let state = DesiredState::default();
        let mut value = serde_json::to_value(&state).unwrap();
        let object = value.as_object_mut().unwrap();
        assert!(object.remove("batch_state").is_some());
        assert!(object.remove("build_state").is_some());
        assert!(object.remove("test_leases").is_some());

        let reloaded: DesiredState = serde_json::from_value(value).unwrap();
        assert_eq!(reloaded.batch_state.next_batch_id, 1);
        assert!(reloaded.batch_state.batches.is_empty());
        assert_eq!(reloaded.build_state.next_build_id, 1);
        assert!(reloaded.build_state.builds.is_empty());
        assert!(reloaded.test_leases.is_empty());
    }

    /// The same rule end to end through the persisted store: a legacy
    /// pre-envelope snapshot without the tracker fields loads through
    /// `with_store` (the #83 envelope loader), and the trackers start
    /// from their defaults.
    #[tokio::test]
    async fn pre_12b2_persisted_snapshot_loads_through_the_envelope_loader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pre12b2.redb");
        let app_id = AppId::new("web", "prod");

        // Fake the exact bytes an older binary persisted: current state
        // serialised, tracker fields stripped, stored without envelope.
        let mut legacy_state = DesiredState::default();
        legacy_state.apps.insert(app_id.clone(), default_spec());
        let mut value = serde_json::to_value(&legacy_state).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("batch_state");
        object.remove("build_state");
        object.remove("test_leases");
        let payload = serde_json::to_vec(&value).unwrap();
        {
            let db = Database::create(&path).unwrap();
            let wtx = db.begin_write().unwrap();
            {
                let mut t = wtx.open_table(SNAPSHOT).unwrap();
                t.insert(SNAP_DATA_KEY, payload.as_slice()).unwrap();
                t.insert(SNAP_INDEX_KEY, 1u64.to_le_bytes().as_slice())
                    .unwrap();
            }
            wtx.commit().unwrap();
        }

        let db = std::sync::Arc::new(Database::create(&path).unwrap());
        let sm = CouncilStateMachine::with_store(db).expect("pre-12b.2 snapshot loads");
        let state = sm.desired_state().await;
        assert!(state.apps.contains_key(&app_id));
        assert_eq!(state.batch_state.next_batch_id, 1);
        assert_eq!(state.build_state.next_build_id, 1);
        assert!(state.test_leases.is_empty());
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
}
