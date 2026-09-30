//! Durable ownership and progress for a managed cluster operation.

use std::io::Read;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::relish::RelishError;
use crate::upgrade::BinaryVersion;

/// Immutable choices for a managed cluster; resuming must preserve them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterSpec {
    /// Human-readable cluster name and trust domain.
    pub name: String,
    /// Either one node or a three-node quorum.
    pub nodes: usize,
    /// Version of the verified binaries installed in every VM.
    pub version: BinaryVersion,
    /// First host API port; subsequent nodes use successive ports.
    pub api_port: u16,
    /// Host port forwarded to the first node's HTTP ingress.
    pub ingress_port: u16,
    /// Host port forwarded to the first node's Pickle registry.
    pub registry_port: u16,
}

impl ClusterSpec {
    fn validate(&self) -> Result<(), RelishError> {
        validate_name(&self.name)?;
        if !matches!(self.nodes, 1 | 3) {
            return Err(failed("quickstart supports one or three nodes"));
        }
        let last = self
            .api_port
            .checked_add(self.nodes as u16 - 1)
            .ok_or_else(|| failed("API port range overflows"))?;
        if self.api_port < 1024
            || self.ingress_port < 1024
            || (self.api_port..=last).contains(&self.ingress_port)
            || self.registry_port < 1024
            || (self.api_port..=last).contains(&self.registry_port)
            || self.registry_port == self.ingress_port
        {
            return Err(failed("managed ports must be unprivileged and distinct"));
        }
        Ok(())
    }
}

/// Last completed provisioning step; live readiness is always checked afresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodePhase {
    /// Ownership was saved before any VM command ran.
    Planned,
    /// The VM exists and has booted.
    Created,
    /// Binaries and configuration have been installed.
    Configured,
    /// The node has a committed cluster identity bundle.
    Enrolled,
    /// The guest service has been started.
    Started,
}

/// An owned VM and its latest known address and provisioning checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeState {
    /// Exact VM name, including this operation's random identifier.
    pub name: String,
    /// Inter-VM address discovered on Lima's shared user network.
    pub address: Option<Ipv4Addr>,
    /// Last successfully completed step.
    pub phase: NodePhase,
}

/// State persisted before creating any external resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterState {
    /// Persistence format, currently 1.
    pub schema: u32,
    /// Random ownership identifier retained across retries.
    pub id: String,
    /// Original creation parameters.
    pub spec: ClusterSpec,
    /// The complete set of owned VM names.
    pub nodes: Vec<NodeState>,
}

impl ClusterState {
    fn validate(&self) -> Result<(), RelishError> {
        self.spec.validate()?;
        if self.schema != 1
            || self.id.len() != 32
            || !self.id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.nodes.len() != self.spec.nodes
        {
            return Err(failed("invalid managed cluster state"));
        }
        for (index, node) in self.nodes.iter().enumerate() {
            if node.name != vm_name(&self.id, index) {
                return Err(failed(
                    "managed state contains a VM not owned by this operation",
                ));
            }
        }
        Ok(())
    }
}

/// An exclusively locked operation. Dropping it releases the process lock.
pub struct Operation {
    /// Canonical directory containing checkpoints and private bootstrap material.
    pub directory: PathBuf,
    /// Current checkpoints; call `save` after a completed external step.
    pub state: ClusterState,
    _lock: std::sync::Arc<OperationLock>,
}

/// The `flock` on an operation's `operation.lock`, released when dropped.
///
/// A `flock` belongs to the open file description, not to the descriptor, and
/// a child process that another thread is spawning holds a copy of every
/// descriptor until its `exec` closes it. Closing our descriptor alone would
/// leave the lock held by that copy for a moment, so a reopen straight after a
/// drop could be refused (#285). Unlocking explicitly releases it for every
/// copy at once.
struct OperationLock(std::fs::File);

impl Drop for OperationLock {
    fn drop(&mut self) {
        // Nothing useful can be done with a failed unlock: closing the
        // descriptor straight after still releases the lock eventually.
        let _ = self.0.unlock();
    }
}

impl Operation {
    /// Begin or resume an operation, refusing changed topology or version.
    pub fn open(root: &Path, spec: &ClusterSpec) -> Result<Self, RelishError> {
        spec.validate()?;
        let (directory, lock) = lock_directory(root, &spec.name, true)?;
        let path = directory.join("state.json");
        let state = if path.exists() {
            let state = read_state(&path)?;
            if state.spec != *spec {
                return Err(failed(&spec_mismatch(&state.spec, spec)));
            }
            state
        } else {
            let id = format!("{:032x}", rand::random::<u128>());
            let nodes = (0..spec.nodes)
                .map(|index| NodeState {
                    name: vm_name(&id, index),
                    address: None,
                    phase: NodePhase::Planned,
                })
                .collect();
            ClusterState {
                schema: 1,
                id,
                spec: spec.clone(),
                nodes,
            }
        };
        let operation = Self {
            directory,
            state,
            _lock: std::sync::Arc::new(OperationLock(lock)),
        };
        operation.save()?;
        Ok(operation)
    }

    /// Lock an existing operation for lifecycle commands without changing its version.
    pub fn load(root: &Path, name: &str) -> Result<Self, RelishError> {
        let (directory, lock) = lock_directory(root, name, false)?;
        let state = read_state(&directory.join("state.json"))?;
        if state.spec.name != name {
            return Err(failed(
                "managed cluster name does not match its state directory",
            ));
        }
        Ok(Self {
            directory,
            state,
            _lock: std::sync::Arc::new(OperationLock(lock)),
        })
    }

    /// Durably replace the checkpoint after validating resource ownership.
    pub fn save(&self) -> Result<(), RelishError> {
        save_state(&self.directory, &self.state)
    }

    /// Persist a checkpoint off the async runtime, retaining the operation lock
    /// until the write finishes even if the awaiting task is cancelled.
    pub async fn save_async(&self) -> Result<(), RelishError> {
        let directory = self.directory.clone();
        let state = self.state.clone();
        let lock = std::sync::Arc::clone(&self._lock);
        tokio::task::spawn_blocking(move || {
            let _lock = lock;
            save_state(&directory, &state)
        })
        .await
        .map_err(|error| failed(&format!("checkpoint task failed: {error}")))?
    }
}

fn save_state(directory: &Path, state: &ClusterState) -> Result<(), RelishError> {
    state.validate()?;
    let bytes = serde_json::to_vec_pretty(state).map_err(RelishError::SerialiseJson)?;
    crate::sesame::identity::atomic_write_mode(&directory.join("state.json"), &bytes, Some(0o600))?;
    Ok(())
}

/// Why a saved cluster can't be resumed with the requested parameters.
///
/// A newer installer over an older laptop cluster is the common case, and
/// before 1.0 the answer is usually a fresh cluster, so a version change
/// gets its own message naming both versions and the command.
fn spec_mismatch(saved: &ClusterSpec, requested: &ClusterSpec) -> String {
    if saved.version != requested.version {
        let name = if saved.name == "laptop" {
            String::new()
        } else {
            format!(" --name {}", saved.name)
        };
        return format!(
            "cluster {:?} was set up with {}, and this installer is {}. Before 1.0, a release that \
             changes the cluster's protocol or state format can't take over an older cluster: run \
             `relish local destroy{name} --yes` and set it up again, then re-apply your apps. \
             See {}",
            saved.name,
            saved.version,
            requested.version,
            crate::compatibility::POLICY_URL
        );
    }
    let mut changes = Vec::new();
    let mut compare = |label: &str, saved: String, requested: String| {
        if saved != requested {
            changes.push(format!("{label}: saved {saved}, requested {requested}"));
        }
    };
    compare(
        "nodes",
        saved.nodes.to_string(),
        requested.nodes.to_string(),
    );
    compare(
        "API port",
        saved.api_port.to_string(),
        requested.api_port.to_string(),
    );
    compare(
        "ingress port",
        saved.ingress_port.to_string(),
        requested.ingress_port.to_string(),
    );
    compare(
        "registry port",
        saved.registry_port.to_string(),
        requested.registry_port.to_string(),
    );
    format!(
        "existing cluster parameters differ ({}); resume with the original parameters",
        changes.join("; ")
    )
}

fn vm_name(id: &str, index: usize) -> String {
    format!("rb-{}-{}", &id[..12], index + 1)
}

fn validate_name(name: &str) -> Result<(), RelishError> {
    if name.is_empty()
        || name.len() > 32
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(failed(
            "cluster name must contain 1-32 lowercase letters, digits or hyphens",
        ));
    }
    crate::config::node::ClusterSection {
        name: name.to_string(),
        ..Default::default()
    }
    .validate()
    .map_err(|error| failed(&error.to_string()))
}

fn lock_directory(
    root: &Path,
    name: &str,
    create: bool,
) -> Result<(PathBuf, std::fs::File), RelishError> {
    validate_name(name)?;
    let directory = root.join("clusters").join(name);
    if create {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory)?;
    }
    let directory = std::fs::canonicalize(directory)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(directory.join("operation.lock"))?;
    lock.try_lock().map_err(|error| {
        failed(&format!(
            "another operation is using cluster {name}: {error}"
        ))
    })?;
    Ok((directory, lock))
}

fn read_state(path: &Path) -> Result<ClusterState, RelishError> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(65537)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(failed("managed state exceeds 64 KiB"));
    }
    let state: ClusterState = serde_json::from_slice(&bytes)
        .map_err(|error| failed(&format!("invalid managed state: {error}")))?;
    state.validate()?;
    Ok(state)
}

fn failed(message: &str) -> RelishError {
    RelishError::InitFailed(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_cluster_names_do_not_expand_vm_socket_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut spec = spec();
        spec.name = "a".repeat(32);
        let operation = Operation::open(root.path(), &spec).unwrap();
        assert!(
            operation
                .state
                .nodes
                .iter()
                .all(|node| node.name.len() <= 20)
        );
    }

    fn spec() -> ClusterSpec {
        ClusterSpec {
            name: "local".to_string(),
            nodes: 3,
            version: "v0.1.0".parse().unwrap(),
            api_port: 19117,
            ingress_port: 18080,
            registry_port: 15050,
        }
    }

    #[test]
    fn reopening_an_operation_preserves_its_identity_and_owned_vm_names() {
        let root = tempfile::tempdir().unwrap();
        let first = Operation::open(root.path(), &spec()).unwrap();
        let id = first.state.id.clone();
        let names: Vec<_> = first
            .state
            .nodes
            .iter()
            .map(|node| node.name.clone())
            .collect();
        assert_eq!(names.len(), 3);
        drop(first);
        let resumed = Operation::open(root.path(), &spec()).unwrap();
        assert_eq!(resumed.state.id, id);
        assert_eq!(
            resumed
                .state
                .nodes
                .iter()
                .map(|node| node.name.clone())
                .collect::<Vec<_>>(),
            names
        );
    }

    #[test]
    fn a_second_writer_cannot_enter_the_same_cluster_operation() {
        let root = tempfile::tempdir().unwrap();
        let first = Operation::open(root.path(), &spec()).unwrap();
        assert!(Operation::open(root.path(), &spec()).is_err());
        drop(first);
        assert!(Operation::open(root.path(), &spec()).is_ok());
    }

    #[test]
    fn resuming_cannot_silently_resize_or_upgrade_the_cluster() {
        let root = tempfile::tempdir().unwrap();
        drop(Operation::open(root.path(), &spec()).unwrap());
        let mut changed = spec();
        changed.nodes = 1;
        assert!(Operation::open(root.path(), &changed).is_err());
        changed = spec();
        changed.version = "v0.2.0".parse().unwrap();
        assert!(Operation::open(root.path(), &changed).is_err());
    }

    #[test]
    fn a_newer_installer_names_both_versions_and_the_fresh_cluster_remedy() {
        let root = tempfile::tempdir().unwrap();
        let mut saved = spec();
        saved.name = "laptop".to_string();
        drop(Operation::open(root.path(), &saved).unwrap());
        let mut requested = saved.clone();
        requested.version = "v0.1.1".parse().unwrap();
        let message = Operation::open(root.path(), &requested)
            .err()
            .unwrap()
            .to_string();
        assert!(
            message.contains("set up with v0.1.0") && message.contains("this installer is v0.1.1"),
            "{message}"
        );
        assert!(message.contains("relish local destroy --yes"), "{message}");
        assert!(message.contains("Before 1.0"), "{message}");
    }

    #[test]
    fn a_named_cluster_is_destroyed_by_name() {
        let root = tempfile::tempdir().unwrap();
        drop(Operation::open(root.path(), &spec()).unwrap());
        let mut requested = spec();
        requested.version = "v0.1.1".parse().unwrap();
        let message = Operation::open(root.path(), &requested)
            .err()
            .unwrap()
            .to_string();
        assert!(
            message.contains("relish local destroy --name local --yes"),
            "{message}"
        );
    }

    #[test]
    fn other_changed_parameters_are_named_with_their_saved_values() {
        let root = tempfile::tempdir().unwrap();
        drop(Operation::open(root.path(), &spec()).unwrap());
        let mut requested = spec();
        requested.nodes = 1;
        requested.api_port = 29117;
        let message = Operation::open(root.path(), &requested)
            .err()
            .unwrap()
            .to_string();
        assert!(
            message.contains("nodes: saved 3, requested 1")
                && message.contains("API port: saved 19117, requested 29117"),
            "{message}"
        );
        assert!(message.contains("original parameters"), "{message}");
    }

    #[tokio::test]
    async fn progress_is_persisted_before_the_next_step() {
        let root = tempfile::tempdir().unwrap();
        let mut operation = Operation::open(root.path(), &spec()).unwrap();
        operation.state.nodes[0].phase = NodePhase::Created;
        operation.save_async().await.unwrap();
        drop(operation);
        let resumed = Operation::open(root.path(), &spec()).unwrap();
        assert_eq!(resumed.state.nodes[0].phase, NodePhase::Created);
    }

    #[test]
    fn an_edited_state_cannot_claim_an_unrelated_vm() {
        let root = tempfile::tempdir().unwrap();
        let mut operation = Operation::open(root.path(), &spec()).unwrap();
        operation.state.nodes[0].name = "reliaburger-test".to_string();
        assert!(operation.save().is_err());
    }

    #[test]
    fn invalid_names_topologies_and_conflicting_ports_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        for invalid in [
            ClusterSpec {
                name: "../other".to_string(),
                ..spec()
            },
            ClusterSpec { nodes: 0, ..spec() },
            ClusterSpec { nodes: 2, ..spec() },
            ClusterSpec {
                ingress_port: 19118,
                ..spec()
            },
            ClusterSpec {
                registry_port: 1023,
                ..spec()
            },
            ClusterSpec {
                registry_port: 19118,
                ..spec()
            },
            ClusterSpec {
                registry_port: 18080,
                ..spec()
            },
        ] {
            assert!(Operation::open(root.path(), &invalid).is_err());
        }
    }

    /// #285: while another thread spawns a child, the child briefly shares
    /// every open descriptor, including the lock. Dropping an operation must
    /// still release its lock at once, or the reopen right after is refused.
    #[test]
    fn a_dropped_operation_reopens_while_other_threads_spawn_processes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let root = tempfile::tempdir().unwrap();
        let spawning = Arc::new(AtomicBool::new(true));
        let spawners: Vec<_> = (0..2)
            .map(|_| {
                let spawning = Arc::clone(&spawning);
                std::thread::spawn(move || {
                    while spawning.load(Ordering::Relaxed) {
                        let _ = std::process::Command::new("true")
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status();
                    }
                })
            })
            .collect();
        let refused = (0..100)
            .filter(|_| Operation::open(root.path(), &spec()).is_err())
            .count();
        spawning.store(false, Ordering::Relaxed);
        for spawner in spawners {
            spawner.join().unwrap();
        }
        assert_eq!(refused, 0, "reopens refused by a lock nobody holds");
    }
}
