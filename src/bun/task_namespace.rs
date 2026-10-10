//! Cached namespace ancestry for delegated containers. Bind before create/start;
//! keep bindings while an owner is uncertain, and clear the previous boot's
//! journal only after startup has retired all old delegated runtime owners.
//!
//! A lease also carries the rest of a task's network policy: the egress
//! allowlist of its cgroup and the namespace ownership of its address. Both
//! are released with the lease once the runtime has retired.
use crate::onion::ebpf::loader::OnionEbpf;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;

const MAX_NAMESPACES: usize = 256;
/// How long a task's egress allowlist may take to resolve before the
/// attempt is refused.
const EGRESS_DNS_PATIENCE: std::time::Duration = std::time::Duration::from_secs(5);
#[derive(Default)]
struct Binding {
    cgroup: u64,
    users: usize,
    published: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    boot: String,
    namespaces: BTreeMap<String, u64>,
}

/// Network state the delegated job runtimes hold on this node, shared with
/// the agent. The agent's kernel reconciliation keeps it: task addresses
/// stay in `destination_map` with their jobs' `allow_from` grants, and the
/// sweep leaves task egress allowlists alone.
#[derive(Default)]
pub struct DelegatedNetwork {
    state: Mutex<DelegatedState>,
    changed: AtomicBool,
}

/// A point-in-time copy of [`DelegatedNetwork`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DelegatedState {
    /// Each task container address and who owns it.
    pub addresses: BTreeMap<Ipv4Addr, DelegatedAddress>,
    /// Task cgroups holding an egress allowlist.
    pub egress_cgroups: BTreeSet<u64>,
}

/// The owner of one task container address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegatedAddress {
    /// Namespace of the job the task runs for.
    pub namespace: String,
    /// The runtime instance holding the address; its destination identity.
    pub owner: String,
    /// Sources in other namespaces the job's `allow_from` admits.
    pub allow_from: Option<Vec<String>>,
}

impl DelegatedNetwork {
    /// Copy the current state.
    pub async fn snapshot(&self) -> DelegatedState {
        self.state.lock().await.clone()
    }

    /// Whether the state changed since the last call, so the agent should
    /// reconcile the kernel maps again.
    pub fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::AcqRel)
    }

    async fn update(&self, change: impl FnOnce(&mut DelegatedState)) {
        change(&mut *self.state.lock().await);
        self.changed.store(true, Ordering::Release);
    }
}

/// Shared with the agent's exact kernel map, never a second attached program.
pub struct TaskNamespacePolicy {
    kernel: Arc<Mutex<OnionEbpf>>,
    bindings: Mutex<BTreeMap<String, Binding>>,
    path: PathBuf,
    boot: String,
    network: Arc<DelegatedNetwork>,
    workload_dns: Option<Ipv4Addr>,
}
/// Deliberately has no Drop release: an abandoned runtime still owns its source.
pub struct NamespaceLease {
    policy: Arc<TaskNamespacePolicy>,
    namespace: String,
    egress_cgroup: Option<u64>,
    address: Option<Ipv4Addr>,
}
impl NamespaceLease {
    /// Call only after runtime retirement is confirmed. Lifts the task's
    /// egress allowlist and address ownership before the namespace binding.
    pub async fn retired(self) {
        if let Some(cgroup) = self.egress_cgroup {
            let mut kernel = self.policy.kernel.lock().await;
            if let Err(error) =
                crate::sesame::egress::delete_cgroup_egress_state(&mut kernel.bpf, cgroup)
            {
                // The agent's sweep scrubs an allowlist no owner claims.
                eprintln!("sesame: retired task egress for cgroup {cgroup} remains: {error}");
            }
        }
        if let Some(address) = self.address {
            let mut kernel = self.policy.kernel.lock().await;
            if let Err(error) = crate::sesame::firewall::delete_destination_entry(
                &mut kernel.bpf,
                crate::onion::types::DestinationKey::any_port(address),
            ) {
                eprintln!("sesame: retired task address {address} remains owned: {error}");
            }
        }
        let (cgroup, address) = (self.egress_cgroup, self.address);
        self.policy
            .network
            .update(|state| {
                if let Some(cgroup) = cgroup {
                    state.egress_cgroups.remove(&cgroup);
                }
                if let Some(address) = address {
                    state.addresses.remove(&address);
                }
            })
            .await;
        let mut bindings = self.policy.bindings.lock().await;
        if let Some(binding) = bindings.get_mut(&self.namespace) {
            binding.users -= 1;
        }
    }

    /// Hold the task cgroup at `cgroup` to `allow` (plus the node's DNS
    /// responder) before anything runs in it. A name that doesn't resolve
    /// refuses the attempt rather than starting it deny-all.
    pub async fn enforce_egress(&mut self, cgroup: &Path, allow: &[String]) -> std::io::Result<()> {
        use crate::sesame::egress;
        if allow.is_empty() {
            return Ok(());
        }
        let cgroup_id = egress::cgroup_id_of_path(cgroup)
            .ok_or_else(|| std::io::Error::other("task cgroup identity is unavailable"))?;
        let entries = allow.to_vec();
        let lookup = tokio::task::spawn_blocking(move || egress::resolve_egress_entries(&entries));
        let mut destinations = tokio::time::timeout(EGRESS_DNS_PATIENCE, lookup)
            .await
            .map_err(|_| std::io::Error::other("egress allowlist did not resolve in time"))?
            .map_err(std::io::Error::other)?
            .map_err(std::io::Error::other)?;
        destinations.extend(egress::implicit_destinations(self.policy.workload_dns));
        let merged = egress::merge_cidr_ports(&destinations).map_err(std::io::Error::other)?;
        // Claim the cgroup before writing it, so the sweep never mistakes a
        // half-written allowlist for an abandoned one.
        self.policy
            .network
            .update(|state| {
                state.egress_cgroups.insert(cgroup_id);
            })
            .await;
        self.egress_cgroup = Some(cgroup_id);
        let mut kernel = self.policy.kernel.lock().await;
        if !(kernel.is_attached()
            && kernel.connect6_attached()
            && kernel.sendmsg4_attached()
            && kernel.sendmsg6_attached())
        {
            return Err(std::io::Error::other(
                "egress enforcement needs every connect and sendmsg hook",
            ));
        }
        // Enforced with no entries denies everything, so the flag goes first.
        egress::set_egress_enforced(&mut kernel.bpf, cgroup_id).map_err(std::io::Error::other)?;
        egress::delete_cgroup_egress_entries(&mut kernel.bpf, cgroup_id)
            .map_err(std::io::Error::other)?;
        egress::write_egress_destinations(&mut kernel.bpf, cgroup_id, &destinations, &merged)
            .map_err(std::io::Error::other)
    }

    /// Record that the task's container `address` belongs to this lease's
    /// namespace on every port, with the job's `allow_from` grants.
    pub async fn publish_address(
        &mut self,
        address: Ipv4Addr,
        owner: &str,
        allow_from: Option<Vec<String>>,
    ) -> std::io::Result<()> {
        let namespace = self.namespace.clone();
        let app_id = crate::sesame::firewall::workload_app_id(&namespace, owner);
        let entry = DelegatedAddress {
            namespace: namespace.clone(),
            owner: owner.to_string(),
            allow_from,
        };
        self.policy
            .network
            .update(|state| {
                state.addresses.insert(address, entry);
            })
            .await;
        self.address = Some(address);
        let mut kernel = self.policy.kernel.lock().await;
        crate::sesame::firewall::write_destination_entry(
            &mut kernel.bpf,
            crate::onion::types::DestinationKey::any_port(address),
            crate::onion::types::DestinationValue {
                app_id,
                namespace_id: crate::onion::vip::name_to_id(&namespace),
            },
        )
        .map_err(std::io::Error::other)
    }
}
impl TaskNamespacePolicy {
    /// Preflight authority for namespace-only bindings before application adoption.
    /// This does not remove a source: old delegated owners still need retirement.
    pub(crate) async fn recorded_sources(data: &Path) -> std::io::Result<BTreeMap<u64, u32>> {
        let path = data.join("batch-namespaces.json");
        tokio::task::spawn_blocking(move || {
            let journal = crate::durable::read_json_if_exists::<Journal>(
                &path,
                64 << 10,
                crate::durable::Access::Exclusive,
            )?;
            let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
            let Some(journal) = journal else {
                return Ok(BTreeMap::new());
            };
            recorded_sources(journal, boot.trim(), |namespace| {
                crate::sesame::egress::cgroup_id_of_path(
                    &Path::new("/sys/fs/cgroup/reliaburger").join(namespace),
                )
            })
        })
        .await
        .map_err(std::io::Error::other)?
    }
    /// Startup must first retire old delegated owners. Never erase a live source.
    pub async fn recover(
        kernel: Arc<Mutex<OnionEbpf>>,
        data: &Path,
        network: Arc<DelegatedNetwork>,
        workload_dns: Option<Ipv4Addr>,
    ) -> std::io::Result<Arc<Self>> {
        let path = data.join("batch-namespaces.json");
        let read_path = path.clone();
        let (journal, boot) = tokio::task::spawn_blocking(move || {
            Ok::<_, std::io::Error>((
                crate::durable::read_json_if_exists::<Journal>(
                    &read_path,
                    64 << 10,
                    crate::durable::Access::Exclusive,
                )?,
                std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
                    .trim()
                    .to_string(),
            ))
        })
        .await
        .map_err(std::io::Error::other)??;
        if let Some(journal) = journal {
            if journal.namespaces.len() > MAX_NAMESPACES
                || !crate::grill::process_owner::valid_boot_id(&journal.boot)
                || journal
                    .namespaces
                    .keys()
                    .any(|name| !crate::config::valid_workload_label(name))
            {
                return Err(std::io::Error::other("invalid delegated namespace journal"));
            }
            if journal.boot == boot {
                let mut kernel = kernel.lock().await;
                for (namespace, cgroup) in journal.namespaces {
                    remove_binding(&mut kernel, &namespace, cgroup)?;
                }
            }
        }
        let policy = Arc::new(Self {
            kernel,
            bindings: Mutex::new(BTreeMap::new()),
            path,
            boot,
            network,
            workload_dns,
        });
        policy.persist(&BTreeMap::new()).await?;
        Ok(policy)
    }
    async fn persist(&self, bindings: &BTreeMap<String, Binding>) -> std::io::Result<()> {
        let journal = Journal {
            boot: self.boot.clone(),
            namespaces: bindings
                .iter()
                .map(|(name, binding)| (name.clone(), binding.cgroup))
                .collect(),
        };
        let bytes = serde_json::to_vec(&journal)?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            crate::sesame::identity::atomic_write_mode(&path, &bytes, Some(0o600))
        })
        .await
        .map_err(std::io::Error::other)?
    }
    /// Bind the namespace at hierarchy depth two, before any descendant starts.
    /// Exact app bindings still take precedence in the connect hook.
    pub async fn acquire(
        self: &Arc<Self>,
        namespace: &str,
        cgroup: &Path,
    ) -> std::io::Result<NamespaceLease> {
        let directory = cgroup
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| std::io::Error::other("task cgroup has no namespace ancestor"))?;
        if directory.parent() != Some(Path::new("/sys/fs/cgroup/reliaburger"))
            || directory.file_name().and_then(|name| name.to_str()) != Some(namespace)
            || !crate::config::valid_workload_label(namespace)
        {
            return Err(std::io::Error::other(
                "invalid delegated namespace ancestry",
            ));
        }
        tokio::fs::create_dir_all(directory).await?;
        let cgroup_id = crate::sesame::egress::cgroup_id_of_path(directory)
            .ok_or_else(|| std::io::Error::other("delegated namespace identity is unavailable"))?;
        let mut bindings = self.bindings.lock().await;
        if let Some(binding) = bindings.get(namespace) {
            if binding.cgroup != cgroup_id {
                return Err(std::io::Error::other("delegated namespace cgroup changed"));
            }
        } else {
            if bindings.len() == MAX_NAMESPACES {
                let idle = bindings
                    .iter()
                    .find(|(_, binding)| binding.users == 0)
                    .map(|(name, binding)| (name.clone(), binding.cgroup))
                    .ok_or_else(|| {
                        std::io::Error::other("all delegated namespace bindings are occupied")
                    })?;
                {
                    let mut kernel = self.kernel.lock().await;
                    remove_binding(&mut kernel, &idle.0, idle.1)?;
                }
                bindings.remove(&idle.0);
            }
            bindings.insert(
                namespace.into(),
                Binding {
                    cgroup: cgroup_id,
                    users: 0,
                    published: false,
                },
            );
            // Record ownership before publishing; failed persistence never starts work.
            if let Err(error) = self.persist(&bindings).await {
                bindings.remove(namespace);
                return Err(error);
            }
        }
        let mut kernel = self.kernel.lock().await;
        if !kernel.is_attached() {
            return Err(std::io::Error::other(
                "delegated namespace enforcement is unavailable",
            ));
        }
        let binding = bindings
            .get_mut(namespace)
            .ok_or_else(|| std::io::Error::other("delegated namespace source is missing"))?;
        if !binding.published {
            crate::sesame::firewall::write_cgroup_namespace_entry(
                &mut kernel.bpf,
                cgroup_id,
                crate::onion::vip::name_to_id(namespace),
            )
            .map_err(std::io::Error::other)?;
            binding.published = true;
        }
        binding.users += 1;
        Ok(NamespaceLease {
            policy: self.clone(),
            namespace: namespace.into(),
            egress_cgroup: None,
            address: None,
        })
    }
    /// A lost source binding must stop the original owner before another attempt.
    pub async fn check(&self, namespace: &str) -> std::io::Result<()> {
        let bindings = self.bindings.lock().await;
        let binding = bindings
            .get(namespace)
            .ok_or_else(|| std::io::Error::other("delegated namespace source is missing"))?;
        let mut kernel = self.kernel.lock().await;
        let observed =
            crate::sesame::firewall::read_firewall_state(&mut kernel.bpf, binding.cgroup, 0)
                .map_err(std::io::Error::other)?
                .source_namespace_id;
        if !kernel.is_attached() || observed != Some(crate::onion::vip::name_to_id(namespace)) {
            return Err(std::io::Error::other(
                "delegated namespace enforcement was lost",
            ));
        }
        Ok(())
    }
}
fn remove_binding(kernel: &mut OnionEbpf, namespace: &str, cgroup: u64) -> std::io::Result<()> {
    let observed = crate::sesame::firewall::read_firewall_state(&mut kernel.bpf, cgroup, 0)
        .map_err(std::io::Error::other)?
        .source_namespace_id;
    if let Some(observed) = observed {
        if observed != crate::onion::vip::name_to_id(namespace) {
            return Err(std::io::Error::other(
                "delegated namespace binding belongs to another source",
            ));
        }
        crate::sesame::firewall::delete_cgroup_namespace_entry(&mut kernel.bpf, cgroup)
            .map_err(std::io::Error::other)?;
    }
    Ok(())
}

/// Namespace roots cannot authorise application firewall rules or other inodes.
fn recorded_sources(
    journal: Journal,
    boot: &str,
    lookup: impl Fn(&str) -> Option<u64>,
) -> std::io::Result<BTreeMap<u64, u32>> {
    if journal.namespaces.len() > MAX_NAMESPACES
        || !crate::grill::process_owner::valid_boot_id(&journal.boot)
        || journal
            .namespaces
            .keys()
            .any(|name| !crate::config::valid_workload_label(name))
        || journal.namespaces.values().any(|id| *id == 0)
    {
        return Err(std::io::Error::other("invalid delegated namespace journal"));
    }
    if journal.boot != boot {
        return Ok(BTreeMap::new());
    }
    let mut sources = BTreeMap::new();
    for (namespace, recorded) in journal.namespaces {
        if lookup(&namespace) != Some(recorded)
            || sources
                .insert(recorded, crate::onion::vip::name_to_id(&namespace))
                .is_some()
        {
            return Err(std::io::Error::other(
                "delegated namespace ownership conflicts with its original cgroup",
            ));
        }
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;
    const BOOT: &str = "ac8a4519-2725-482c-844a-1aadb1d673de";
    fn journal() -> Journal {
        Journal {
            boot: BOOT.into(),
            namespaces: BTreeMap::from([("default".into(), 42)]),
        }
    }
    #[test]
    fn recorded_delegated_sources_require_the_original_namespace_inode_and_boot() {
        assert_eq!(
            recorded_sources(journal(), BOOT, |_| Some(42)).unwrap(),
            BTreeMap::from([(42, crate::onion::vip::name_to_id("default"))])
        );
        assert!(recorded_sources(journal(), BOOT, |_| Some(43)).is_err());
        assert!(recorded_sources(journal(), BOOT, |_| None).is_err());
        assert!(
            recorded_sources(journal(), "11111111-2222-3333-4444-555555555555", |_| None)
                .unwrap()
                .is_empty()
        );
        let mut duplicate = journal();
        duplicate.namespaces.insert("other".into(), 42);
        assert!(recorded_sources(duplicate, BOOT, |_| Some(42)).is_err());
        let mut malformed = journal();
        malformed.boot = "unknown".into();
        assert!(recorded_sources(malformed, BOOT, |_| Some(42)).is_err());
    }
}
