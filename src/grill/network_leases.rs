//! Durable ownership of the rootful runtime's finite container address pool.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Result;
use std::net::Ipv4Addr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::InstanceId;
use super::netns::{MAX_CONTAINERS_PER_NODE, MAX_NODE_INDEX, gateway_ip, node_index_from_id};
use crate::file_lock::FileLock;

const MAX_JOURNAL_BYTES: u64 = 256 * 1024;

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

/// The node's container subnet index (#2420): the one its lease journal
/// already holds, so it never changes once chosen. At first use it's the
/// hostname's hash, moved on past any index whose /23 overlaps one of
/// `host_networks`, and written to the journal before it's returned.
///
/// It used to be the hostname's hash on every start, so renaming the host
/// changed the index under every live container. Their intents and leases
/// then no longer matched the runtime, and Bun could neither stop them nor
/// start new ones.
pub(crate) fn persisted_node_index(
    directory: &std::path::Path,
    hostname: &str,
    host_networks: &[(Ipv4Addr, u8)],
) -> Result<u16> {
    std::fs::create_dir_all(directory)?;
    let _lock = lock_pool(directory)?;
    let path = directory.join(".network-leases.json");
    if let Some(journal) = crate::durable::read_json_if_exists::<Journal>(
        &path,
        MAX_JOURNAL_BYTES,
        crate::durable::Access::Regular,
    )? {
        if journal.version != 1 || journal.node_index == 0 || journal.node_index > MAX_NODE_INDEX {
            return Err(std::io::Error::other(
                "invalid or incompatible network lease journal",
            ));
        }
        return Ok(journal.node_index);
    }
    let wanted = node_index_from_id(hostname);
    let node_index = (0..MAX_NODE_INDEX)
        .map(|step| (wanted - 1 + step) % MAX_NODE_INDEX + 1)
        .find(|index| {
            let overlapping = host_networks
                .iter()
                .find(|network| overlaps(*index, **network));
            if let Some((address, prefix)) = overlapping {
                let (subnet, length) = subnet_network(*index);
                eprintln!(
                    "bun: container subnet {subnet}/{length} overlaps the host's network \
                     {address}/{prefix}; trying the next one"
                );
            }
            overlapping.is_none()
        })
        .ok_or_else(|| std::io::Error::other("every container subnet overlaps a host network"))?;
    let journal = Journal {
        version: 1,
        node_index,
        allocations: BTreeMap::new(),
    };
    let bytes = serde_json::to_vec(&journal).map_err(std::io::Error::other)?;
    crate::sesame::identity::atomic_write_mode(&path, &bytes, Some(0o600))?;
    Ok(node_index)
}

/// The /23 a node index's containers get addresses from.
fn subnet_network(node_index: u16) -> (Ipv4Addr, u8) {
    let [a, b, c, _] = gateway_ip(node_index).octets();
    (Ipv4Addr::new(a, b, c, 0), 23)
}

/// Whether `node_index`'s /23 shares any address with `network`.
fn overlaps(node_index: u16, network: (Ipv4Addr, u8)) -> bool {
    let (subnet, length) = subnet_network(node_index);
    let prefix = length.min(network.1);
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    u32::from(subnet) & mask == u32::from(network.0) & mask
}

/// The IPv4 networks in a `/proc/net/route` table, apart from the default
/// route. A local address's own subnet is listed there too.
pub(crate) fn host_networks(table: &str) -> Vec<(Ipv4Addr, u8)> {
    // The kernel prints each address as the raw integer in memory, so its
    // native-endian bytes are the address's bytes in order.
    let address = |hex: &str| {
        u32::from_str_radix(hex, 16)
            .ok()
            .map(|raw| Ipv4Addr::from(raw.to_ne_bytes()))
    };
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let destination = address(fields.get(1)?)?;
            let prefix = u32::from(address(fields.get(7)?)?).count_ones() as u8;
            (prefix > 0).then_some((destination, prefix))
        })
        .collect()
}

/// The networks this host routes to, from procfs. Empty where there's no
/// procfs.
pub(crate) fn host_networks_on_this_node() -> Vec<(Ipv4Addr, u8)> {
    std::fs::read_to_string("/proc/net/route")
        .map(|table| host_networks(&table))
        .unwrap_or_default()
}

/// Take the pool's lock, held across every read and write of its journal.
fn lock_pool(directory: &std::path::Path) -> Result<FileLock> {
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
    FileLock::try_lock(lock)
        .map_err(|e| std::io::Error::other(format!("network address pool is busy: {e}")))
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
            let _lock = lock_pool(&directory)?;
            let path = directory.join(".network-leases.json");
            let mut journal = crate::durable::read_json_if_exists::<Journal>(
                &path,
                MAX_JOURNAL_BYTES,
                crate::durable::Access::Regular,
            )?
            .unwrap_or_else(|| Journal {
                version: 1,
                node_index: node,
                allocations: BTreeMap::new(),
            });
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
    async fn concurrent_reservations_never_share_an_address() {
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
        assert!(addresses.contains(&index));
    }

    /// #2420: the subnet index is chosen once and kept. A hostname that
    /// changes later doesn't change it, so live containers' intents and
    /// leases still match their runtime.
    #[test]
    fn the_node_index_survives_a_hostname_change() {
        let tmp = tempfile::tempdir().unwrap();
        let first = persisted_node_index(tmp.path(), "host-a", &[]).unwrap();
        assert_eq!(first, node_index_from_id("host-a"));
        assert_eq!(
            persisted_node_index(tmp.path(), "host-b", &[]).unwrap(),
            first
        );
        let fresh = tempfile::tempdir().unwrap();
        assert_eq!(
            persisted_node_index(fresh.path(), "host-b", &[]).unwrap(),
            node_index_from_id("host-b")
        );
    }

    /// The pinned index is the one the lease journal already holds, so a
    /// node that has run containers keeps their addresses.
    #[tokio::test]
    async fn the_node_index_of_an_existing_lease_journal_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = NetworkLeases::new(tmp.path().to_path_buf());
        pool.reserve(&InstanceId("owner".into()), 7).await.unwrap();
        assert_eq!(
            persisted_node_index(tmp.path(), "anything", &[]).unwrap(),
            7
        );
        assert_eq!(
            pool.lookup(&InstanceId("owner".into()), 7).await.unwrap(),
            Some(0)
        );
    }

    /// At first use, an index whose /23 overlaps a network the host is on
    /// is passed over, so no container address shadows a LAN host.
    #[test]
    fn a_first_index_that_overlaps_a_host_network_is_passed_over() {
        let tmp = tempfile::tempdir().unwrap();
        let wanted = node_index_from_id("host-a");
        let lan = (subnet_network(wanted).0, 16);
        let chosen = persisted_node_index(tmp.path(), "host-a", &[lan]).unwrap();
        assert_ne!(chosen, wanted);
        assert!(!overlaps(chosen, lan));
        // Kept from then on, whatever the routes say.
        assert_eq!(
            persisted_node_index(tmp.path(), "host-a", &[]).unwrap(),
            chosen
        );
    }

    #[test]
    fn host_networks_are_read_from_the_kernel_route_table() {
        // 10.20.0.0/16 on eth0 and the default route, as /proc/net/route
        // writes them: little-endian hex.
        let table =
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
eth0\t00000000\t0100140A\t0003\t0\t0\t100\t00000000\t0\t0\t0
eth0\t0000140A\t00000000\t0001\t0\t0\t100\t0000FFFF\t0\t0\t0
";
        assert_eq!(
            host_networks(table),
            vec![(std::net::Ipv4Addr::new(10, 20, 0, 0), 16)]
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
