//! The user namespace every rootful runc container runs in.
//!
//! Images expect to choose their own user: plenty run as root so their
//! entrypoint can `chown` a data directory and drop to a service account,
//! or bind port 80. We honour that, but "root" inside the container must
//! never be root on the node. Each container gets a user namespace that maps
//! its ids `0..CONTAINER_ID_COUNT` onto the node's subordinate range
//! `HOST_ID_BASE..HOST_ID_BASE + CONTAINER_ID_COUNT`. Container root is host
//! uid 2,000,000,000: an unprivileged user that owns nothing outside the
//! container's own files.
//!
//! Every container on a node shares the one range, the way Docker's
//! `userns-remap` does. That keeps the unpacked image cache shareable: we
//! shift file ownership once, at unpack time, instead of per container.
//! Per-container ranges would need idmapped mounts for each rootfs.

use std::path::{Path, PathBuf};

use super::oci::{OciCapabilities, OciIdMapping, OciNamespace, OciSpec};

/// First host uid (and gid) of the node's container range.
///
/// Above systemd's container range (524288..1879048191) and below its
/// dynamic and foreign ranges near 2^31, so it collides with neither
/// `/etc/subuid` allocations nor systemd-nspawn. Keep it out of any
/// directory service's uid plan.
pub const HOST_ID_BASE: u32 = 2_000_000_000;

/// How many container ids map: 0..65535, the ids images actually use.
pub const CONTAINER_ID_COUNT: u32 = 65_536;

/// Docker's default capability set. Inside a user namespace they only act
/// on what the namespace owns (the container's files, processes and
/// mounts), which is what lets an entrypoint `chown` its data directory and
/// `setuid` to a service user without any host privilege.
const DEFAULT_CAPABILITIES: &[&str] = &[
    "CAP_AUDIT_WRITE",
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_KILL",
    "CAP_MKNOD",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_RAW",
    "CAP_SETFCAP",
    "CAP_SETGID",
    "CAP_SETPCAP",
    "CAP_SETUID",
    "CAP_SYS_CHROOT",
];

/// Why a container can't run in the node's user namespace.
#[derive(Debug, thiserror::Error)]
pub enum UserNamespaceError {
    #[error("container {kind} {id} is outside the mapped range 0..{CONTAINER_ID_COUNT}")]
    IdOutOfRange { kind: &'static str, id: u32 },
}

/// The host uid or gid that container id `id` maps to.
pub fn host_id(id: u32) -> Option<u32> {
    (id < CONTAINER_ID_COUNT).then(|| HOST_ID_BASE + id)
}

/// The container id host uid or gid `id` stands for, if the range maps it.
pub fn container_id(id: u32) -> Option<u32> {
    id.checked_sub(HOST_ID_BASE)
        .filter(|offset| *offset < CONTAINER_ID_COUNT)
}

/// Put a rootful container in the node's user namespace.
///
/// Adds the `user` namespace and its id mappings, grants Docker's default
/// capabilities, and swaps `/sys` for a read-only bind of the host's:
/// the kernel only lets a user namespace mount a fresh sysfs when it also
/// owns the network namespace, and ours is created by the node beforehand.
pub fn apply(spec: &mut OciSpec) -> Result<(), UserNamespaceError> {
    let user = &spec.process.user;
    if host_id(user.uid).is_none() {
        return Err(UserNamespaceError::IdOutOfRange {
            kind: "uid",
            id: user.uid,
        });
    }
    if host_id(user.gid).is_none() {
        return Err(UserNamespaceError::IdOutOfRange {
            kind: "gid",
            id: user.gid,
        });
    }

    if !spec.linux.namespaces.iter().any(|ns| ns.ns_type == "user") {
        spec.linux.namespaces.push(OciNamespace {
            ns_type: "user".to_string(),
            path: None,
        });
    }
    let mapping = vec![OciIdMapping {
        container_id: 0,
        host_id: HOST_ID_BASE,
        size: CONTAINER_ID_COUNT,
    }];
    spec.linux.uid_mappings = Some(mapping.clone());
    spec.linux.gid_mappings = Some(mapping);

    let capabilities: Vec<String> = DEFAULT_CAPABILITIES
        .iter()
        .map(|name| name.to_string())
        .collect();
    spec.process.capabilities = Some(OciCapabilities {
        bounding: capabilities.clone(),
        effective: capabilities.clone(),
        permitted: capabilities,
    });

    bind_host_sys(spec);
    Ok(())
}

/// Replace the spec's `/sys` mount with a read-only bind of the host's.
///
/// Mounting a fresh sysfs needs privilege over the network namespace,
/// which neither a rootless container nor our user-namespaced rootful
/// containers have.
pub fn bind_host_sys(spec: &mut OciSpec) {
    for mount in &mut spec.mounts {
        if mount.destination == Path::new("/sys") {
            mount.source = Some(PathBuf::from("/sys"));
            mount.mount_type = Some("none".to_string());
            mount.options = vec![
                "rbind".to_string(),
                "nosuid".to_string(),
                "noexec".to_string(),
                "nodev".to_string(),
                "ro".to_string(),
            ];
        }
    }
}

/// Hand a bind-mounted directory to the container's user.
///
/// Used for the workload identity directory: its key is owner-only, so it
/// must belong to whichever host uid the container's process maps to.
pub fn chown_to_container_user(path: &Path, spec: &OciSpec) -> std::io::Result<()> {
    let (Some(uid), Some(gid)) = (
        host_id(spec.process.user.uid),
        host_id(spec.process.user.gid),
    ) else {
        return Err(std::io::Error::other("container user outside mapped range"));
    };
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
    for entry in std::fs::read_dir(path)? {
        std::os::unix::fs::lchown(entry?.path(), Some(uid), Some(gid))?;
    }
    Ok(())
}

/// The bind-mount source for `destination`, if the spec has one.
pub fn bind_source<'a>(spec: &'a OciSpec, destination: &Path) -> Option<&'a PathBuf> {
    spec.mounts
        .iter()
        .find(|mount| mount.destination == destination)
        .and_then(|mount| mount.source.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> OciSpec {
        let app: crate::config::app::AppSpec = toml::from_str(r#"image = "redis:8""#).unwrap();
        let mut spec = crate::grill::oci::generate_oci_spec(
            "redis",
            "default",
            &app,
            "default__redis-0",
            None,
            "/sys/fs/cgroup/reliaburger/default/redis-0",
            None,
            None,
        );
        spec.process.user.uid = 0;
        spec.process.user.gid = 0;
        spec
    }

    #[test]
    fn container_ids_map_onto_the_node_range() {
        assert_eq!(host_id(0), Some(2_000_000_000));
        assert_eq!(host_id(999), Some(2_000_000_999));
        assert_eq!(host_id(65_535), Some(2_000_065_535));
        assert_eq!(host_id(65_536), None);
        assert_eq!(container_id(2_000_000_999), Some(999));
        assert_eq!(container_id(2_000_065_536), None);
        assert_eq!(container_id(0), None, "host root is no container id");
        // The whole range stays below 2^31, so no signed-int tool
        // mistakes a container uid for a negative number.
        assert!(u64::from(HOST_ID_BASE) + u64::from(CONTAINER_ID_COUNT) < 1 << 31);
    }

    #[test]
    fn container_root_is_never_host_root() {
        let mut spec = spec();
        apply(&mut spec).unwrap();
        assert!(spec.linux.namespaces.iter().any(|ns| ns.ns_type == "user"));
        for mappings in [&spec.linux.uid_mappings, &spec.linux.gid_mappings] {
            let mappings = mappings.as_ref().unwrap();
            assert_eq!(mappings.len(), 1);
            assert_eq!(mappings[0].container_id, 0);
            assert_eq!(mappings[0].host_id, HOST_ID_BASE);
            assert_eq!(mappings[0].size, CONTAINER_ID_COUNT);
        }
    }

    #[test]
    fn user_namespace_grants_docker_default_capabilities() {
        let mut spec = spec();
        apply(&mut spec).unwrap();
        let capabilities = spec.process.capabilities.unwrap();
        for wanted in [
            "CAP_CHOWN",
            "CAP_SETUID",
            "CAP_SETGID",
            "CAP_NET_BIND_SERVICE",
        ] {
            assert!(capabilities.effective.iter().any(|cap| cap == wanted));
            assert!(capabilities.bounding.iter().any(|cap| cap == wanted));
        }
        for refused in ["CAP_SYS_ADMIN", "CAP_NET_ADMIN", "CAP_SYS_PTRACE"] {
            assert!(!capabilities.bounding.iter().any(|cap| cap == refused));
        }
    }

    #[test]
    fn sysfs_becomes_a_read_only_host_bind() {
        let mut spec = spec();
        apply(&mut spec).unwrap();
        let sys = spec
            .mounts
            .iter()
            .find(|mount| mount.destination == Path::new("/sys"))
            .unwrap();
        assert_eq!(sys.source.as_deref(), Some(Path::new("/sys")));
        assert!(sys.options.iter().any(|option| option == "rbind"));
        assert!(sys.options.iter().any(|option| option == "ro"));
    }

    #[test]
    fn applying_twice_adds_one_user_namespace() {
        let mut spec = spec();
        apply(&mut spec).unwrap();
        apply(&mut spec).unwrap();
        let user_namespaces = spec
            .linux
            .namespaces
            .iter()
            .filter(|ns| ns.ns_type == "user")
            .count();
        assert_eq!(user_namespaces, 1);
    }

    #[test]
    fn ids_outside_the_range_are_refused() {
        let mut spec = spec();
        spec.process.user.uid = 70_000;
        assert!(matches!(
            apply(&mut spec),
            Err(UserNamespaceError::IdOutOfRange { kind: "uid", .. })
        ));
        let mut spec = self::spec();
        spec.process.user.gid = 1 << 20;
        assert!(matches!(
            apply(&mut spec),
            Err(UserNamespaceError::IdOutOfRange { kind: "gid", .. })
        ));
    }
}
