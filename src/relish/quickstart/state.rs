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
            let legacy_name = format!("rb-{}-{}-{}", self.spec.name, &self.id[..12], index + 1);
            if node.name != vm_name(&self.id, index) && node.name != legacy_name {
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
    _lock: std::sync::Arc<std::fs::File>,
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
                return Err(failed(
                    "existing cluster parameters differ; resume with the original parameters",
                ));
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
            _lock: std::sync::Arc::new(lock),
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
            _lock: std::sync::Arc::new(lock),
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
        ] {
            assert!(Operation::open(root.path(), &invalid).is_err());
        }
    }
}
