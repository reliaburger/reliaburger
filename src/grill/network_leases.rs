//! Durable ownership of the rootful runtime's finite container address pool.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Result};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::InstanceId;
use super::netns::{MAX_CONTAINERS_PER_NODE, MAX_NODE_INDEX};

const MAX_JOURNAL_BYTES: u64 = 256 * 1024;

// Closing the parent descriptor alone can leave a lock held briefly by a
// concurrently forked child before exec. Explicit unlock retires that ownership.
struct JournalLock(std::fs::File);

impl Drop for JournalLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    node_index: u16,
    allocations: BTreeMap<String, u16>,
}

/// Reservations survive cancellation and process replacement. Callers may retire
/// a reservation only after confirming its kernel resources have disappeared.
#[derive(Clone)]
pub(crate) struct NetworkLeases {
    directory: PathBuf,
    transaction: Arc<Mutex<()>>,
}

impl NetworkLeases {
    /// Open a lazily loaded pool underneath the runtime's bundle directory.
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            transaction: Arc::new(Mutex::new(())),
        }
    }

    /// Persist exclusive ownership before any network mutation. Duplicate owners
    /// must be adopted or retired explicitly, never treated as a new creation.
    pub(crate) async fn reserve(&self, instance: &InstanceId, node: u16) -> Result<u16> {
        if instance.0.is_empty() || instance.0.len() > 256 {
            return Err(std::io::Error::other("invalid network reservation owner"));
        }
        let owner = instance.0.clone();
        self.transact(node, move |journal| {
            if journal.allocations.contains_key(&owner) {
                return Err(std::io::Error::other(
                    "instance already owns a network address; adopt or retire it first",
                ));
            }
            let occupied: BTreeSet<_> = journal.allocations.values().copied().collect();
            let index = (0..MAX_CONTAINERS_PER_NODE)
                .find(|index| !occupied.contains(index))
                .ok_or_else(|| std::io::Error::other("container address pool exhausted"))?;
            journal.allocations.insert(owner, index);
            Ok((index, true))
        })
        .await
    }

    /// Retain a verified live address across process replacement, refusing any
    /// different owner or disagreement with the persisted reservation.
    pub(crate) async fn adopt(&self, instance: &InstanceId, node: u16, index: u16) -> Result<()> {
        if instance.0.is_empty() || instance.0.len() > 256 {
            return Err(std::io::Error::other("invalid network reservation owner"));
        }
        let owner = instance.0.clone();
        self.transact(node, move |journal| {
            if index >= MAX_CONTAINERS_PER_NODE {
                return Err(std::io::Error::other(
                    "adopted address is outside the container pool",
                ));
            }
            if let Some(existing) = journal.allocations.get(&owner) {
                return if *existing == index {
                    Ok(((), false))
                } else {
                    Err(std::io::Error::other(
                        "adopted address conflicts with the instance reservation",
                    ))
                };
            }
            if journal.allocations.values().any(|used| *used == index) {
                return Err(std::io::Error::other(
                    "adopted address belongs to another instance",
                ));
            }
            journal.allocations.insert(owner, index);
            Ok(((), true))
        })
        .await
    }

    /// Recover an owner's plan, including a cancelled or partially failed setup.
    pub(crate) async fn lookup(&self, instance: &InstanceId, node: u16) -> Result<Option<u16>> {
        if instance.0.is_empty() || instance.0.len() > 256 {
            return Err(std::io::Error::other("invalid network reservation owner"));
        }
        let owner = instance.0.clone();
        self.transact(node, move |journal| {
            Ok((journal.allocations.get(&owner).copied(), false))
        })
        .await
    }

    /// Release this exact reservation after the caller has verified teardown.
    pub(crate) async fn retire(&self, instance: &InstanceId, node: u16, index: u16) -> Result<()> {
        let owner = instance.0.clone();
        self.transact(node, move |journal| match journal.allocations.get(&owner) {
            Some(current) if *current == index => {
                journal.allocations.remove(&owner);
                Ok(((), true))
            }
            None => Ok(((), false)),
            Some(_) => Err(std::io::Error::other(
                "cannot retire a different network reservation",
            )),
        })
        .await
    }

    async fn transact<T: Send + 'static>(
        &self,
        node: u16,
        operation: impl FnOnce(&mut Journal) -> Result<(T, bool)> + Send + 'static,
    ) -> Result<T> {
        if node == 0 || node > MAX_NODE_INDEX {
            return Err(std::io::Error::other("invalid node subnet index"));
        }
        let directory = self.directory.clone();
        let guard = self.transaction.clone().lock_owned().await;
        // The task owns the mutex and OS lock through persistence, even when its
        // async caller is cancelled. Every operation reloads after uncertain I/O.
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            std::fs::create_dir_all(&directory)?;
            if let Some(parent) = directory.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::File::open(parent)?.sync_all()?;
            }
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(directory.join(".network-leases.lock"))?;
            if !lock.metadata()?.is_file() {
                return Err(std::io::Error::other(
                    "network lease lock is not a regular file",
                ));
            }
            lock.try_lock()
                .map_err(|e| std::io::Error::other(format!("network address pool is busy: {e}")))?;
            let _lock = JournalLock(lock);
            let path = directory.join(".network-leases.json");
            let mut journal = match std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)
            {
                Ok(file) => {
                    if !file.metadata()?.is_file() {
                        return Err(std::io::Error::other(
                            "network lease journal is not a regular file",
                        ));
                    }
                    let mut bytes = Vec::new();
                    file.take(MAX_JOURNAL_BYTES + 1).read_to_end(&mut bytes)?;
                    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
                        return Err(std::io::Error::other("network lease journal is too large"));
                    }
                    serde_json::from_slice::<Journal>(&bytes).map_err(std::io::Error::other)?
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Journal {
                    version: 1,
                    node_index: node,
                    allocations: BTreeMap::new(),
                },
                Err(e) => return Err(e),
            };
            let unique: BTreeSet<_> = journal.allocations.values().copied().collect();
            if journal.version != 1
                || journal.node_index != node
                || unique.len() != journal.allocations.len()
                || unique.iter().any(|index| *index >= MAX_CONTAINERS_PER_NODE)
                || journal
                    .allocations
                    .keys()
                    .any(|owner| owner.is_empty() || owner.len() > 256)
            {
                return Err(std::io::Error::other(
                    "invalid or incompatible network lease journal",
                ));
            }
            let (result, changed) = operation(&mut journal)?;
            if changed {
                let bytes = serde_json::to_vec(&journal).map_err(std::io::Error::other)?;
                if bytes.len() as u64 > MAX_JOURNAL_BYTES {
                    return Err(std::io::Error::other("network lease journal is too large"));
                }
                crate::sesame::identity::atomic_write_mode(&path, &bytes, Some(0o600))?;
            }
            Ok(result)
        })
        .await
        .map_err(std::io::Error::other)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pool_exhaustion_retirement_and_restart_preserve_ownership() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = NetworkLeases::new(tmp.path().to_path_buf());
        for index in 0..MAX_CONTAINERS_PER_NODE {
            assert_eq!(
                pool.reserve(&InstanceId(format!("owner-{index}")), 1)
                    .await
                    .unwrap(),
                index
            );
        }
        let extra = InstanceId("extra".into());
        assert!(
            pool.reserve(&extra, 1)
                .await
                .unwrap_err()
                .to_string()
                .contains("exhausted")
        );
        drop(pool);
        let pool = NetworkLeases::new(tmp.path().to_path_buf());
        assert!(pool.reserve(&extra, 1).await.is_err());
        let owner = InstanceId("owner-37".into());
        assert_eq!(pool.lookup(&owner, 1).await.unwrap(), Some(37));
        assert!(pool.retire(&owner, 1, 38).await.is_err());
        pool.retire(&owner, 1, 37).await.unwrap();
        assert_eq!(pool.reserve(&extra, 1).await.unwrap(), 37);
    }

    #[tokio::test]
    async fn concurrent_reservations_and_adoption_never_share_an_address() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = NetworkLeases::new(tmp.path().to_path_buf());
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..32 {
            let pool = pool.clone();
            tasks.spawn(async move {
                pool.reserve(&InstanceId(format!("owner-{index}")), 1)
                    .await
                    .unwrap()
            });
        }
        let mut addresses = BTreeSet::new();
        while let Some(result) = tasks.join_next().await {
            assert!(addresses.insert(result.unwrap()));
        }
        let owner = InstanceId("owner-0".into());
        let index = pool.lookup(&owner, 1).await.unwrap().unwrap();
        assert!(pool.reserve(&owner, 1).await.is_err());
        pool.adopt(&owner, 1, index).await.unwrap();
        assert!(pool.adopt(&owner, 1, 508).await.is_err());
        assert!(
            pool.adopt(&InstanceId("other".into()), 1, index)
                .await
                .is_err()
        );
        pool.adopt(&InstanceId("recovered".into()), 1, 508)
            .await
            .unwrap();
        assert!(
            pool.adopt(&InstanceId("outside".into()), 1, 509)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn changed_subnet_corrupt_journal_and_symlink_refuse_without_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = NetworkLeases::new(tmp.path().to_path_buf());
        let owner = InstanceId("owner".into());
        pool.reserve(&owner, 1).await.unwrap();
        assert!(pool.reserve(&InstanceId("other".into()), 2).await.is_err());
        let path = tmp.path().join(".network-leases.json");
        std::fs::write(&path, b"broken").unwrap();
        assert!(pool.reserve(&owner, 1).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"broken");
        std::fs::remove_file(&path).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::write(&outside, b"preserve").unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(pool.reserve(&owner, 1).await.is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"preserve");
    }
}
