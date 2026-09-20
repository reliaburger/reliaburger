//! Durable egress ownership published before any kernel policy mutation.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::grill::{InstanceId, oci::OciSpec, records::RuntimeKind};
use crate::sesame::egress::EgressDestination;

const CHECKPOINT_FILE: &str = "egress-owners.checkpoint";
const MAX_CHECKPOINT_BYTES: u64 = 16 * 1024 * 1024;

/// Kernel identity, independent of the Bun process or any workload PID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct KernelBootId([u8; 16]);

/// Positive cleanup evidence survives until the adoption record is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum PolicyPhase {
    /// Kernel cleanup remains an obligation of this original owner.
    Owned,
    /// Kernel cleanup is confirmed; adoption metadata may still remain.
    Retired,
}

/// Original authority for one instance's kernel policy, independent of adoption.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EgressBinding {
    /// Whether this owner still has a kernel cleanup obligation.
    pub phase: PolicyPhase,
    /// Original kernel cgroup identity, retained even after runtime deletion.
    pub cgroup_id: u64,
    /// Original allowlist, re-resolved periodically while the instance is live.
    pub allow: Vec<String>,
    /// Runtime responsible for proving execution and retirement.
    pub runtime: RuntimeKind,
    /// Original launch input used to correlate the complete runtime inventory.
    pub original_spec: OciSpec,
    /// Kernel boot identity; cgroup numbers cannot be compared across boots.
    pub boot_id: KernelBootId,
    /// Current resolution is transient. Recovery resolves the original policy
    /// afresh while retaining the existing enforcement flag.
    #[serde(skip)]
    pub resolved: Vec<EgressDestination>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    instance_id: InstanceId,
    binding: EgressBinding,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    schema: u32,
    owners: Vec<Owner>,
}

fn validate(owners: &HashMap<InstanceId, EgressBinding>) -> io::Result<()> {
    for (id, owner) in owners {
        let path = owner
            .original_spec
            .linux
            .cgroups_path
            .as_deref()
            .map(Path::new);
        if id.0.is_empty()
            || !id
                .0
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            || owner.cgroup_id == 0
            || owner.boot_id.0 == [0; 16]
            || owner.allow.is_empty()
            || owner.allow.iter().any(String::is_empty)
            || path.is_none_or(|path| {
                !path.is_absolute()
                    || path.components().any(|component| {
                        matches!(
                            component,
                            std::path::Component::ParentDir | std::path::Component::CurDir
                        )
                    })
            })
        {
            return Err(io::Error::other("invalid durable egress ownership"));
        }
    }
    Ok(())
}

/// Read every original owner before adoption or kernel cleanup. Only a missing
/// checkpoint denotes no owners; invalid or partial inventories refuse recovery.
pub(super) fn load(directory: &Path) -> io::Result<HashMap<InstanceId, EgressBinding>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    let file = match options.open(directory.join(CHECKPOINT_FILE)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_CHECKPOINT_BYTES {
        return Err(io::Error::other("invalid egress ownership checkpoint file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o777 != 0o600
            || metadata.uid() != nix::unistd::geteuid().as_raw()
            || metadata.nlink() != 1
        {
            return Err(io::Error::other(
                "egress ownership checkpoint is not private",
            ));
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_CHECKPOINT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
        return Err(io::Error::other("egress ownership checkpoint is too large"));
    }
    let checkpoint: Checkpoint = serde_json::from_slice(&bytes)?;
    if checkpoint.schema != 1 {
        return Err(io::Error::other("unsupported egress ownership schema"));
    }
    let mut owners = HashMap::new();
    for owner in checkpoint.owners {
        if owners.insert(owner.instance_id, owner.binding).is_some() {
            return Err(io::Error::other("duplicate egress ownership identity"));
        }
    }
    validate(&owners)?;
    Ok(owners)
}

/// Durably replace the complete inventory before allowing policy mutation.
#[cfg(any(test, all(feature = "ebpf", target_os = "linux")))]
pub(super) fn persist(
    directory: &Path,
    owners: HashMap<InstanceId, EgressBinding>,
) -> io::Result<()> {
    validate(&owners)?;
    let mut owners: Vec<_> = owners
        .into_iter()
        .map(|(instance_id, binding)| Owner {
            instance_id,
            binding,
        })
        .collect();
    owners.sort_by(|left, right| left.instance_id.0.cmp(&right.instance_id.0));
    let bytes = serde_json::to_vec(&Checkpoint { schema: 1, owners })?;
    if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
        return Err(io::Error::other("egress ownership checkpoint is too large"));
    }
    std::fs::create_dir_all(directory)?;
    crate::sesame::identity::atomic_write_mode(
        &directory.join(CHECKPOINT_FILE),
        &bytes,
        Some(0o600),
    )?;
    if let Some(parent) = directory.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// Positive identity of the running kernel, never inferred from PID reuse.
#[cfg(all(feature = "ebpf", target_os = "linux"))]
pub(super) fn boot_id() -> io::Result<KernelBootId> {
    let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let value = value.trim();
    if value.len() != 36
        || value.bytes().enumerate().any(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte != b'-'
            } else {
                !byte.is_ascii_hexdigit()
            }
        })
    {
        return Err(io::Error::other("kernel boot identity is invalid"));
    }
    let mut bytes = [0; 16];
    hex::decode_to_slice(value.replace('-', ""), &mut bytes).map_err(io::Error::other)?;
    if bytes == [0; 16] {
        return Err(io::Error::other("kernel boot identity is unavailable"));
    }
    Ok(KernelBootId(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owners() -> HashMap<InstanceId, EgressBinding> {
        let original_spec = crate::grill::oci::generate_oci_spec(
            "web",
            "default",
            &crate::config::Config::parse("[app.web]\nimage = 'example:v1'\n")
                .unwrap()
                .app["web"],
            "default__web-0",
            None,
            "/sys/fs/cgroup/reliaburger/default/web/0",
            None,
            None,
        );
        HashMap::from([(
            InstanceId("default__web-0".into()),
            EgressBinding {
                phase: PolicyPhase::Owned,
                cgroup_id: 42,
                allow: vec!["203.0.113.1:443".into()],
                runtime: RuntimeKind::Runc,
                original_spec,
                boot_id: KernelBootId([1; 16]),
                resolved: Vec::new(),
            },
        )])
    }

    #[test]
    fn original_policy_ownership_survives_checkpoint_replacement() {
        let root = tempfile::tempdir().unwrap();
        assert!(load(root.path()).unwrap().is_empty());
        let expected = owners();
        persist(root.path(), expected.clone()).unwrap();
        assert_eq!(load(root.path()).unwrap(), expected);
        persist(root.path(), HashMap::new()).unwrap();
        assert!(load(root.path()).unwrap().is_empty());
    }

    #[test]
    fn damaged_or_duplicate_policy_inventory_refuses_recovery() {
        let root = tempfile::tempdir().unwrap();
        persist(root.path(), owners()).unwrap();
        let path = root.path().join(CHECKPOINT_FILE);
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let first = document["owners"][0].clone();
        document["owners"].as_array_mut().unwrap().push(first);
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        assert!(load(root.path()).is_err());
        std::fs::write(&path, b"{").unwrap();
        assert!(load(root.path()).is_err());
    }

    #[test]
    fn invalid_policy_identity_never_replaces_the_original_inventory() {
        let root = tempfile::tempdir().unwrap();
        let original = owners();
        persist(root.path(), original.clone()).unwrap();
        let mut invalid = original.clone();
        invalid.values_mut().next().unwrap().cgroup_id = 0;
        assert!(persist(root.path(), invalid).is_err());
        assert_eq!(load(root.path()).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn policy_checkpoint_refuses_redirected_or_public_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = tempfile::tempdir().unwrap();
        persist(root.path(), owners()).unwrap();
        let path = root.path().join(CHECKPOINT_FILE);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(root.path()).is_err());
        let redirected = root.path().join("redirected");
        std::fs::rename(&path, &redirected).unwrap();
        symlink(redirected, path).unwrap();
        assert!(load(root.path()).is_err());
    }
}
