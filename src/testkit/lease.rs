//! Durable ownership records for Phase 15-created resources.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::meat::AppId;

/// Current persisted and API schema version for test leases.
pub const TEST_LEASE_SCHEMA_VERSION: u32 = 1;

/// Maximum live lease records accepted by one standalone node or cluster.
pub const MAX_ACTIVE_TEST_LEASES: usize = 64;

/// Maximum resources which one lease may own.
pub const MAX_LEASED_RESOURCES: usize = 128;

const CLEANUP_STEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const MAX_LOCAL_LEASE_STORE_BYTES: usize = 4 * 1024 * 1024;

/// A resource whose lifecycle belongs to one server-side lease.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum LeasedResource {
    /// A declarative or local application.
    App { app_id: AppId },
}

/// Whether a lease accepts new resources or is being reclaimed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum TestLeaseState {
    /// The owner may renew the lease and attach resources.
    Active,
    /// Cleanup has started. The record stays durable until every resource is
    /// confirmed absent, so process death resumes rather than forgets it.
    Cleaning {
        /// Number of cleanup attempts started.
        attempts: u32,
        /// Latest bounded failure reason, if an attempt was inconclusive.
        last_error: Option<String>,
    },
}

/// Durable server-owned lifetime for one isolated test namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestLease {
    /// Wire and persistence schema version.
    pub schema_version: u32,
    /// Random, unguessable lease identifier.
    pub lease_id: String,
    /// Stable fingerprint of the exact credential which created the lease.
    pub owner_id: String,
    /// Human-readable token name retained for audit output.
    pub owner_name: String,
    /// Dedicated DNS-label namespace. Always starts with `rbtest-`.
    pub namespace: String,
    /// Current lifecycle state.
    pub state: TestLeaseState,
    /// Creation time.
    pub issued_at_unix_ms: u64,
    /// Hard server-side expiry.
    pub expires_at_unix_ms: u64,
    /// Resources atomically associated with this lease.
    pub resources: BTreeSet<LeasedResource>,
}

impl TestLease {
    /// Build a validated active lease from server-selected values.
    pub fn new(
        lease_id: String,
        owner_id: String,
        owner_name: String,
        namespace: String,
        issued_at_unix_ms: u64,
        expires_at_unix_ms: u64,
    ) -> Result<Self, LeaseError> {
        if lease_id.is_empty() {
            return Err(LeaseError::InvalidId);
        }
        if owner_id.is_empty() || owner_name.is_empty() {
            return Err(LeaseError::InvalidOwner);
        }
        if !valid_test_namespace(&namespace) {
            return Err(LeaseError::InvalidNamespace);
        }
        if expires_at_unix_ms <= issued_at_unix_ms {
            return Err(LeaseError::InvalidExpiry);
        }
        Ok(Self {
            schema_version: TEST_LEASE_SCHEMA_VERSION,
            lease_id,
            owner_id,
            owner_name,
            namespace,
            state: TestLeaseState::Active,
            issued_at_unix_ms,
            expires_at_unix_ms,
            resources: BTreeSet::new(),
        })
    }

    /// Whether the lease may still accept a mutation at `now_unix_ms`.
    pub fn is_active_at(&self, now_unix_ms: u64) -> bool {
        matches!(self.state, TestLeaseState::Active) && now_unix_ms < self.expires_at_unix_ms
    }

    /// Validate a deserialised or replicated record before accepting it.
    pub fn validate(&self) -> Result<(), LeaseError> {
        if self.schema_version != TEST_LEASE_SCHEMA_VERSION {
            return Err(LeaseError::UnsupportedSchema {
                found: self.schema_version,
            });
        }
        if self.lease_id.is_empty() {
            return Err(LeaseError::InvalidId);
        }
        if self.owner_id.is_empty() || self.owner_name.is_empty() {
            return Err(LeaseError::InvalidOwner);
        }
        if !valid_test_namespace(&self.namespace) {
            return Err(LeaseError::InvalidNamespace);
        }
        if self.expires_at_unix_ms <= self.issued_at_unix_ms {
            return Err(LeaseError::InvalidExpiry);
        }
        if self.resources.iter().any(|resource| match resource {
            LeasedResource::App { app_id } => app_id.namespace != self.namespace,
        }) {
            return Err(LeaseError::NamespaceMismatch);
        }
        if self.resources.len() > MAX_LEASED_RESOURCES {
            return Err(LeaseError::ResourceLimit);
        }
        Ok(())
    }

    /// Check ownership and active lifetime before attaching a resource.
    pub fn authorise_owner(&self, owner_id: &str, now_unix_ms: u64) -> Result<(), LeaseError> {
        if self.owner_id != owner_id {
            return Err(LeaseError::WrongOwner);
        }
        if !self.is_active_at(now_unix_ms) {
            return Err(LeaseError::NotActive);
        }
        Ok(())
    }
}

/// Return whether a namespace is reserved and safe for lease-wide cleanup.
pub fn valid_test_namespace(namespace: &str) -> bool {
    namespace.starts_with("rbtest-")
        && namespace.len() <= 63
        && namespace
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !namespace.ends_with('-')
}

/// Lease lifecycle or persistence failure.
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("lease schema {found} is unsupported")]
    UnsupportedSchema { found: u32 },
    #[error("lease id is empty")]
    InvalidId,
    #[error("lease owner is empty")]
    InvalidOwner,
    #[error("lease namespace must be a DNS label beginning with rbtest-")]
    InvalidNamespace,
    #[error("lease expiry must be after its issue time")]
    InvalidExpiry,
    #[error("lease already exists")]
    AlreadyExists,
    #[error("lease namespace is already owned")]
    NamespaceOwned,
    #[error("too many active test leases")]
    TooManyLeases,
    #[error("lease not found")]
    NotFound,
    #[error("lease belongs to another principal")]
    WrongOwner,
    #[error("lease is expired or cleanup has started")]
    NotActive,
    #[error("app namespace does not match its lease")]
    NamespaceMismatch,
    #[error("lease resource limit reached")]
    ResourceLimit,
    #[error("local lease store exceeds 4 MiB")]
    StoreTooLarge,
    #[error("lease persistence failed: {0}")]
    Persistence(#[from] std::io::Error),
    #[error("lease state is malformed: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("lease cleanup failed: {0}")]
    Cleanup(String),
    #[error("lease consensus write failed: {0}")]
    Consensus(String),
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LocalLeaseFile {
    #[serde(default)]
    leases: BTreeMap<String, TestLease>,
}

#[derive(Debug)]
struct LocalLeaseInner {
    leases: BTreeMap<String, TestLease>,
    operation_locks: BTreeMap<String, Arc<Mutex<()>>>,
    path: Option<PathBuf>,
}

/// Proof that cleanup cannot overtake an in-flight standalone lease mutation.
///
/// Dropping this value lets the lease reaper proceed. The guard is deliberately
/// opaque: callers may keep it alive, but cannot mutate lease state through it.
pub struct LocalLeaseOperation {
    _guard: OwnedMutexGuard<()>,
}

/// Standalone Bun's durable lease store.
///
/// Cluster mode keeps the same records in Raft. This store covers a Bun
/// restart in standalone mode and also provides an in-memory test adapter.
#[derive(Clone, Debug)]
pub struct LocalLeaseStore {
    inner: Arc<Mutex<LocalLeaseInner>>,
}

impl LocalLeaseStore {
    /// Build an ephemeral store for embedded routers and unit tests.
    pub fn in_memory() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LocalLeaseInner {
                leases: BTreeMap::new(),
                operation_locks: BTreeMap::new(),
                path: None,
            })),
        }
    }

    /// Open a durable store, refusing malformed state rather than forgetting
    /// resources which may still be live.
    pub async fn open(path: PathBuf) -> Result<Self, LeaseError> {
        let leases = match tokio::fs::read(&path).await {
            Ok(bytes) if bytes.len() > MAX_LOCAL_LEASE_STORE_BYTES => {
                return Err(LeaseError::StoreTooLarge);
            }
            Ok(bytes) => serde_json::from_slice::<LocalLeaseFile>(&bytes)?.leases,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        for lease in leases.values() {
            lease.validate()?;
        }
        if leases.len() > MAX_ACTIVE_TEST_LEASES {
            return Err(LeaseError::TooManyLeases);
        }
        let unique_namespaces: BTreeSet<&str> = leases
            .values()
            .map(|lease| lease.namespace.as_str())
            .collect();
        if unique_namespaces.len() != leases.len() {
            return Err(LeaseError::NamespaceOwned);
        }
        let operation_locks = leases
            .keys()
            .map(|lease_id| (lease_id.clone(), Arc::new(Mutex::new(()))))
            .collect();
        Ok(Self {
            inner: Arc::new(Mutex::new(LocalLeaseInner {
                leases,
                operation_locks,
                path: Some(path),
            })),
        })
    }

    /// Insert a new lease and durably record it before returning.
    pub async fn create(&self, lease: TestLease) -> Result<(), LeaseError> {
        lease.validate()?;
        let mut inner = self.inner.lock().await;
        if inner.leases.contains_key(&lease.lease_id) {
            return Err(LeaseError::AlreadyExists);
        }
        if inner.leases.len() >= MAX_ACTIVE_TEST_LEASES {
            return Err(LeaseError::TooManyLeases);
        }
        if inner
            .leases
            .values()
            .any(|existing| existing.namespace == lease.namespace)
        {
            return Err(LeaseError::NamespaceOwned);
        }
        let lease_id = lease.lease_id.clone();
        let mut next = inner.leases.clone();
        next.insert(lease_id.clone(), lease);
        persist_leases(inner.path.as_deref(), &next).await?;
        inner.leases = next;
        inner
            .operation_locks
            .insert(lease_id, Arc::new(Mutex::new(())));
        Ok(())
    }

    /// Read one current lease.
    pub async fn get(&self, lease_id: &str) -> Option<TestLease> {
        self.inner.lock().await.leases.get(lease_id).cloned()
    }

    /// Attach an app atomically with the durable lease record.
    pub async fn attach_app(
        &self,
        lease_id: &str,
        owner_id: &str,
        app_id: AppId,
        now_unix_ms: u64,
    ) -> Result<(), LeaseError> {
        self.begin_app_operation(lease_id, owner_id, vec![app_id], now_unix_ms)
            .await?;
        Ok(())
    }

    /// Durably attach all apps and prevent cleanup from overtaking their
    /// deployment. The caller keeps the returned guard until the agent closes
    /// its event channel.
    pub async fn begin_app_operation(
        &self,
        lease_id: &str,
        owner_id: &str,
        app_ids: Vec<AppId>,
        now_unix_ms: u64,
    ) -> Result<LocalLeaseOperation, LeaseError> {
        let operation_lock = self.operation_lock(lease_id).await?;
        let operation_guard = operation_lock.lock_owned().await;
        let mut inner = self.inner.lock().await;
        let mut next = inner.leases.clone();
        let lease = next.get_mut(lease_id).ok_or(LeaseError::NotFound)?;
        lease.authorise_owner(owner_id, now_unix_ms)?;
        let resources: BTreeSet<_> = app_ids
            .into_iter()
            .map(|app_id| {
                if app_id.namespace != lease.namespace {
                    Err(LeaseError::NamespaceMismatch)
                } else {
                    Ok(LeasedResource::App { app_id })
                }
            })
            .collect::<Result<_, _>>()?;
        let additional = resources.difference(&lease.resources).count();
        if lease.resources.len().saturating_add(additional) > MAX_LEASED_RESOURCES {
            return Err(LeaseError::ResourceLimit);
        }
        lease.resources.extend(resources);
        persist_leases(inner.path.as_deref(), &next).await?;
        inner.leases = next;
        Ok(LocalLeaseOperation {
            _guard: operation_guard,
        })
    }

    /// Extend an active lease without letting a caller revive an expired one.
    pub async fn renew(
        &self,
        lease_id: &str,
        owner_id: &str,
        now_unix_ms: u64,
        expires_at_unix_ms: u64,
    ) -> Result<TestLease, LeaseError> {
        let mut inner = self.inner.lock().await;
        let mut next = inner.leases.clone();
        let lease = next.get_mut(lease_id).ok_or(LeaseError::NotFound)?;
        lease.authorise_owner(owner_id, now_unix_ms)?;
        if expires_at_unix_ms <= now_unix_ms {
            return Err(LeaseError::InvalidExpiry);
        }
        lease.expires_at_unix_ms = expires_at_unix_ms;
        let result = lease.clone();
        persist_leases(inner.path.as_deref(), &next).await?;
        inner.leases = next;
        Ok(result)
    }

    /// Mark a lease for cleanup. Repeated calls resume the same cleanup.
    pub async fn begin_cleanup(
        &self,
        lease_id: &str,
        owner_id: Option<&str>,
    ) -> Result<TestLease, LeaseError> {
        let (lease, _operation) = self.begin_cleanup_operation(lease_id, owner_id).await?;
        Ok(lease)
    }

    async fn begin_cleanup_operation(
        &self,
        lease_id: &str,
        owner_id: Option<&str>,
    ) -> Result<(TestLease, LocalLeaseOperation), LeaseError> {
        let operation_lock = self.operation_lock(lease_id).await?;
        let operation_guard = operation_lock.lock_owned().await;
        let mut inner = self.inner.lock().await;
        let mut next = inner.leases.clone();
        let lease = next.get_mut(lease_id).ok_or(LeaseError::NotFound)?;
        if owner_id.is_some_and(|owner_id| owner_id != lease.owner_id) {
            return Err(LeaseError::WrongOwner);
        }
        let attempts = match lease.state {
            TestLeaseState::Active => 1,
            TestLeaseState::Cleaning { attempts, .. } => attempts.saturating_add(1),
        };
        lease.state = TestLeaseState::Cleaning {
            attempts,
            last_error: None,
        };
        let result = lease.clone();
        persist_leases(inner.path.as_deref(), &next).await?;
        inner.leases = next;
        Ok((
            result,
            LocalLeaseOperation {
                _guard: operation_guard,
            },
        ))
    }

    /// Retain an inconclusive cleanup for the next reaper attempt.
    pub async fn cleanup_failed(&self, lease_id: &str, reason: &str) -> Result<(), LeaseError> {
        let mut inner = self.inner.lock().await;
        let mut next = inner.leases.clone();
        let lease = next.get_mut(lease_id).ok_or(LeaseError::NotFound)?;
        let attempts = match lease.state {
            TestLeaseState::Cleaning { attempts, .. } => attempts,
            TestLeaseState::Active => return Err(LeaseError::NotActive),
        };
        lease.state = TestLeaseState::Cleaning {
            attempts,
            last_error: Some(reason.chars().take(512).collect()),
        };
        persist_leases(inner.path.as_deref(), &next).await?;
        inner.leases = next;
        Ok(())
    }

    /// Delete a lease only after its caller confirmed cleanup.
    pub async fn finish_cleanup(&self, lease_id: &str) -> Result<(), LeaseError> {
        let mut inner = self.inner.lock().await;
        let lease = inner.leases.get(lease_id).ok_or(LeaseError::NotFound)?;
        if !matches!(lease.state, TestLeaseState::Cleaning { .. }) {
            return Err(LeaseError::NotActive);
        }
        let mut next = inner.leases.clone();
        next.remove(lease_id);
        persist_leases(inner.path.as_deref(), &next).await?;
        inner.leases = next;
        inner.operation_locks.remove(lease_id);
        Ok(())
    }

    /// Active expired and interrupted-cleanup records which the reaper owns.
    pub async fn due_for_cleanup(&self, now_unix_ms: u64) -> Vec<String> {
        self.inner
            .lock()
            .await
            .leases
            .values()
            .filter(|lease| {
                lease.expires_at_unix_ms <= now_unix_ms
                    || matches!(lease.state, TestLeaseState::Cleaning { .. })
            })
            .map(|lease| lease.lease_id.clone())
            .collect()
    }

    async fn operation_lock(&self, lease_id: &str) -> Result<Arc<Mutex<()>>, LeaseError> {
        self.inner
            .lock()
            .await
            .operation_locks
            .get(lease_id)
            .cloned()
            .ok_or(LeaseError::NotFound)
    }
}

impl Default for LocalLeaseStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

async fn persist_leases(
    path: Option<&Path>,
    leases: &BTreeMap<String, TestLease>,
) -> Result<(), LeaseError> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temporary = temporary_path(path);
    let bytes = serde_json::to_vec_pretty(&LocalLeaseFile {
        leases: leases.clone(),
    })?;
    if bytes.len() > MAX_LOCAL_LEASE_STORE_BYTES {
        return Err(LeaseError::StoreTooLarge);
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).await?;
    file.write_all(&bytes).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&temporary, path).await?;
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Current wall-clock time for lease API and reaper decisions.
pub fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Reclaim one standalone lease, retaining its durable record on uncertainty.
pub async fn cleanup_local_lease(
    store: &LocalLeaseStore,
    cmd_tx: &tokio::sync::mpsc::Sender<crate::bun::agent::AgentCommand>,
    lease_id: &str,
    owner_id: Option<&str>,
) -> Result<(), LeaseError> {
    let (lease, _operation) = store.begin_cleanup_operation(lease_id, owner_id).await?;
    for resource in lease.resources {
        match resource {
            LeasedResource::App { app_id } => {
                let (response_tx, response_rx) = tokio::sync::oneshot::channel();
                let cleanup = async {
                    cmd_tx
                        .send(crate::bun::agent::AgentCommand::Stop {
                            app_name: app_id.name,
                            namespace: app_id.namespace,
                            response: response_tx,
                        })
                        .await
                        .map_err(|_| {
                            LeaseError::Cleanup("agent command channel is closed".to_string())
                        })?;
                    response_rx.await.map_err(|_| {
                        LeaseError::Cleanup("agent dropped the cleanup response".to_string())
                    })
                };
                let result = tokio::time::timeout(CLEANUP_STEP_TIMEOUT, cleanup).await;
                match result {
                    Ok(Ok(Ok(()))) | Ok(Ok(Err(crate::bun::BunError::AppNotFound { .. }))) => {}
                    Ok(Ok(Err(error))) => {
                        let reason = error.to_string();
                        store.cleanup_failed(lease_id, &reason).await?;
                        return Err(LeaseError::Cleanup(reason));
                    }
                    Ok(Err(error)) => {
                        store.cleanup_failed(lease_id, &error.to_string()).await?;
                        return Err(error);
                    }
                    Err(_) => {
                        let reason = "agent cleanup timed out";
                        store.cleanup_failed(lease_id, reason).await?;
                        return Err(LeaseError::Cleanup(reason.to_string()));
                    }
                }
            }
        }
    }
    store.finish_cleanup(lease_id).await
}

/// Reclaim one replicated lease through Raft. Every step is idempotent; a
/// leader failure leaves `Cleaning` plus the complete resource list for its
/// successor to resume.
pub async fn cleanup_cluster_lease(
    council: &crate::council::CouncilNode,
    lease_id: &str,
    owner_id: Option<&str>,
) -> Result<(), LeaseError> {
    let before = council.desired_state().await;
    let lease = before
        .test_leases
        .get(lease_id)
        .ok_or(LeaseError::NotFound)?;
    if owner_id.is_some_and(|owner_id| owner_id != lease.owner_id) {
        return Err(LeaseError::WrongOwner);
    }
    if let Err(error) = write_cluster_lease_request(
        council,
        crate::council::RaftRequest::TestLeaseBeginCleanup {
            lease_id: lease_id.to_string(),
        },
    )
    .await
    {
        if !council
            .desired_state()
            .await
            .test_leases
            .contains_key(lease_id)
        {
            return Ok(());
        }
        return Err(error);
    }
    for resource in &lease.resources {
        match resource {
            LeasedResource::App { app_id } => {
                if let Err(error) = write_cluster_lease_request(
                    council,
                    crate::council::RaftRequest::AppDelete {
                        app_id: app_id.clone(),
                    },
                )
                .await
                {
                    record_cluster_cleanup_failure(council, lease_id, &error).await;
                    return Err(error);
                }
            }
        }
    }
    let result = write_cluster_lease_request(
        council,
        crate::council::RaftRequest::TestLeaseFinishCleanup {
            lease_id: lease_id.to_string(),
        },
    )
    .await;
    if result.is_err()
        && !council
            .desired_state()
            .await
            .test_leases
            .contains_key(lease_id)
    {
        return Ok(());
    }
    if let Err(error) = &result {
        record_cluster_cleanup_failure(council, lease_id, error).await;
    }
    result
}

async fn record_cluster_cleanup_failure(
    council: &crate::council::CouncilNode,
    lease_id: &str,
    error: &LeaseError,
) {
    let _ = write_cluster_lease_request(
        council,
        crate::council::RaftRequest::TestLeaseCleanupFailed {
            lease_id: lease_id.to_string(),
            reason: error.to_string().chars().take(512).collect(),
        },
    )
    .await;
}

async fn write_cluster_lease_request(
    council: &crate::council::CouncilNode,
    request: crate::council::RaftRequest,
) -> Result<(), LeaseError> {
    let response = tokio::time::timeout(CLEANUP_STEP_TIMEOUT, council.write(request))
        .await
        .map_err(|_| LeaseError::Consensus("consensus write timed out".to_string()))?;
    match response {
        Ok(crate::council::CouncilResponse::Refused { reason }) => {
            Err(LeaseError::Consensus(reason))
        }
        Ok(_) => Ok(()),
        Err(error) => Err(LeaseError::Consensus(error.to_string())),
    }
}

/// Start the standalone expiry reaper. Cancellation stops new attempts but
/// leaves any incomplete record durable for the next Bun process.
pub fn spawn_local_lease_reaper(
    store: LocalLeaseStore,
    cmd_tx: tokio::sync::mpsc::Sender<crate::bun::agent::AgentCommand>,
    shutdown: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = interval.tick() => {}
            }
            for lease_id in store.due_for_cleanup(now_unix_millis()).await {
                if let Err(error) = cleanup_local_lease(&store, &cmd_tx, &lease_id, None).await {
                    eprintln!("bun: test lease {lease_id} cleanup deferred: {error}");
                }
            }
        }
    })
}

/// Start one reaper on every cluster node. Only the current Raft leader acts;
/// leadership transfer therefore resumes persisted cleanup automatically.
pub fn spawn_cluster_lease_reaper(
    council: Arc<crate::council::CouncilNode>,
    shutdown: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = interval.tick() => {}
            }
            if !council.is_leader().await {
                continue;
            }
            let now = now_unix_millis();
            let due: Vec<String> = council
                .desired_state()
                .await
                .test_leases
                .values()
                .filter(|lease| {
                    lease.expires_at_unix_ms <= now
                        || matches!(lease.state, TestLeaseState::Cleaning { .. })
                })
                .map(|lease| lease.lease_id.clone())
                .collect();
            for lease_id in due {
                if let Err(error) = cleanup_cluster_lease(&council, &lease_id, None).await {
                    eprintln!("bun: cluster test lease {lease_id} cleanup deferred: {error}");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(id: &str, now: u64, expires: u64) -> TestLease {
        TestLease::new(
            id.to_string(),
            "token:ci".to_string(),
            "ci".to_string(),
            format!("rbtest-{id}"),
            now,
            expires,
        )
        .unwrap()
    }

    #[test]
    fn namespaces_are_reserved_dns_labels() {
        assert!(valid_test_namespace("rbtest-a1b2"));
        assert!(!valid_test_namespace("production"));
        assert!(!valid_test_namespace("rbtest-UPPER"));
        assert!(!valid_test_namespace("rbtest-trailing-"));
    }

    #[tokio::test]
    async fn durable_store_survives_restart_with_owned_resources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("leases.json");
        let store = LocalLeaseStore::open(path.clone()).await.unwrap();
        store.create(lease("abc", 10, 20)).await.unwrap();
        store
            .attach_app("abc", "token:ci", AppId::new("web", "rbtest-abc"), 11)
            .await
            .unwrap();
        drop(store);

        let reopened = LocalLeaseStore::open(path).await.unwrap();
        let found = reopened.get("abc").await.unwrap();
        assert_eq!(found.resources.len(), 1);
        assert_eq!(reopened.due_for_cleanup(21).await, vec!["abc"]);
    }

    #[tokio::test]
    async fn failed_persistence_does_not_publish_an_in_memory_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        tokio::fs::write(&blocker, b"block persistence")
            .await
            .unwrap();
        let store = LocalLeaseStore {
            inner: Arc::new(Mutex::new(LocalLeaseInner {
                leases: BTreeMap::new(),
                operation_locks: BTreeMap::new(),
                path: Some(blocker.join("leases.json")),
            })),
        };

        assert!(matches!(
            store.create(lease("abc", 10, 20)).await,
            Err(LeaseError::Persistence(_))
        ));
        assert!(store.get("abc").await.is_none());
    }

    #[tokio::test]
    async fn cleanup_cannot_overtake_an_in_flight_app_operation() {
        let store = LocalLeaseStore::in_memory();
        store.create(lease("abc", 10, 20)).await.unwrap();
        let operation = store
            .begin_app_operation("abc", "token:ci", vec![AppId::new("web", "rbtest-abc")], 11)
            .await
            .unwrap();

        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                store.begin_cleanup("abc", None),
            )
            .await
            .is_err()
        );
        drop(operation);
        assert!(store.begin_cleanup("abc", None).await.is_ok());
    }

    #[tokio::test]
    async fn expired_or_wrong_owner_cannot_attach_or_renew() {
        let store = LocalLeaseStore::in_memory();
        store.create(lease("abc", 10, 20)).await.unwrap();
        assert!(matches!(
            store
                .attach_app("abc", "token:other", AppId::new("web", "rbtest-abc"), 11,)
                .await,
            Err(LeaseError::WrongOwner)
        ));
        assert!(matches!(
            store.renew("abc", "token:ci", 20, 30).await,
            Err(LeaseError::NotActive)
        ));
    }

    #[tokio::test]
    async fn duplicate_namespaces_and_unbounded_lease_counts_are_refused() {
        let store = LocalLeaseStore::in_memory();
        store.create(lease("first", 10, 20)).await.unwrap();
        let duplicate = TestLease::new(
            "second".to_string(),
            "token:ci".to_string(),
            "ci".to_string(),
            "rbtest-first".to_string(),
            10,
            20,
        )
        .unwrap();
        assert!(matches!(
            store.create(duplicate).await,
            Err(LeaseError::NamespaceOwned)
        ));

        for index in 1..MAX_ACTIVE_TEST_LEASES {
            store
                .create(lease(&format!("lease{index}"), 10, 20))
                .await
                .unwrap();
        }
        assert!(matches!(
            store.create(lease("overflow", 10, 20)).await,
            Err(LeaseError::TooManyLeases)
        ));
    }

    #[tokio::test]
    async fn one_lease_cannot_accumulate_unbounded_resources() {
        let store = LocalLeaseStore::in_memory();
        store.create(lease("abc", 10, 20)).await.unwrap();
        for index in 0..MAX_LEASED_RESOURCES {
            store
                .attach_app(
                    "abc",
                    "token:ci",
                    AppId::new(format!("app-{index}"), "rbtest-abc"),
                    11,
                )
                .await
                .unwrap();
        }
        assert!(matches!(
            store
                .attach_app("abc", "token:ci", AppId::new("overflow", "rbtest-abc"), 11,)
                .await,
            Err(LeaseError::ResourceLimit)
        ));
    }

    #[tokio::test]
    async fn failed_cleanup_stays_durable_and_retryable() {
        let store = LocalLeaseStore::in_memory();
        store.create(lease("abc", 10, 20)).await.unwrap();
        store.begin_cleanup("abc", Some("token:ci")).await.unwrap();
        store
            .cleanup_failed("abc", "agent unavailable")
            .await
            .unwrap();
        assert_eq!(store.due_for_cleanup(11).await, vec!["abc"]);
        let retry = store.begin_cleanup("abc", None).await.unwrap();
        assert!(matches!(
            retry.state,
            TestLeaseState::Cleaning { attempts: 2, .. }
        ));
        store.finish_cleanup("abc").await.unwrap();
        assert!(store.get("abc").await.is_none());
    }

    #[tokio::test]
    async fn restarted_standalone_reaper_finishes_an_expired_lease() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("leases.json");
        let store = LocalLeaseStore::open(path.clone()).await.unwrap();
        store.create(lease("abc", 1, 2)).await.unwrap();
        drop(store);

        let reopened = LocalLeaseStore::open(path).await.unwrap();
        let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let task = spawn_local_lease_reaper(reopened.clone(), command_tx, shutdown.clone());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if reopened.get("abc").await.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        shutdown.cancel();
        task.await.unwrap();
    }
}
