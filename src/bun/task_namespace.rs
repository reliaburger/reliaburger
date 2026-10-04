//! Cached namespace ancestry for delegated containers. Bind before create/start;
//! keep bindings while an owner is uncertain, and clear the previous boot's
//! journal only after startup has retired all old delegated runtime owners.
use crate::onion::ebpf::loader::OnionEbpf;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

const MAX_NAMESPACES: usize = 256;
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

/// Shared with the agent's exact kernel map, never a second attached program.
pub struct TaskNamespacePolicy {
    kernel: Arc<Mutex<OnionEbpf>>,
    bindings: Mutex<BTreeMap<String, Binding>>,
    path: PathBuf,
    boot: String,
}
/// Deliberately has no Drop release: an abandoned runtime still owns its source.
pub struct NamespaceLease {
    policy: Arc<TaskNamespacePolicy>,
    namespace: String,
}
impl NamespaceLease {
    /// Call only after runtime retirement is confirmed.
    pub async fn retired(self) {
        let mut bindings = self.policy.bindings.lock().await;
        if let Some(binding) = bindings.get_mut(&self.namespace) {
            binding.users -= 1;
        }
    }
}
impl TaskNamespacePolicy {
    /// Startup must first retire old delegated owners. Never erase a live source.
    pub async fn recover(kernel: Arc<Mutex<OnionEbpf>>, data: &Path) -> std::io::Result<Arc<Self>> {
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
