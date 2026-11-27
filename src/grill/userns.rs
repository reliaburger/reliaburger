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

use super::oci::{OciCapabilities, OciIdMapping, OciMount, OciNamespace, OciSpec};

/// First host uid (and gid) of the node's container range.
///
/// Above systemd's container range (524288..1879048191) and below its
/// dynamic and foreign ranges near 2^31, so it collides with neither
/// `/etc/subuid` allocations nor systemd-nspawn. Keep it out of any
/// directory service's uid plan.
pub const HOST_ID_BASE: u32 = 2_000_000_000;

/// How many container ids map: 0..65535, the ids images actually use.
pub const CONTAINER_ID_COUNT: u32 = 65_536;
/// A helper-only identity outside all permitted image users.
pub const EXECUTOR_CONTAINER_UID: u32 = 65_536;
/// Host delegation identity, absent from ordinary containers' uid mappings.
pub const EXECUTOR_HOST_UID: u32 = 2_100_000_000;

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

/// What runc hides from a container: the `/proc` and `/sys` files that
/// describe or reach the node's hardware and kernel rather than the
/// container. Docker's and containerd's default list, plus the node's
/// security and BPF file systems, which the read-only bind of the host's
/// `/sys` would otherwise show. A file becomes `/dev/null` and a directory an
/// empty tmpfs; a path the kernel doesn't have is skipped.
const MASKED_PATHS: &[&str] = &[
    "/proc/acpi",
    "/proc/asound",
    "/proc/interrupts",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/sched_debug",
    "/proc/scsi",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/sys/devices/virtual/powercap",
    "/sys/firmware",
    "/sys/fs/bpf",
    "/sys/kernel/security",
];

/// What runc makes read-only inside a container: the `/proc` files that
/// change kernel settings or poke the kernel directly (Docker's list).
const READONLY_PATHS: &[&str] = &[
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
];

/// Why a container can't run in the node's user namespace.
#[derive(Debug, thiserror::Error)]
pub enum UserNamespaceError {
    #[error("container {kind} {id} is outside the mapped range 0..{CONTAINER_ID_COUNT}")]
    IdOutOfRange { kind: &'static str, id: u32 },
    #[error(transparent)]
    Seccomp(#[from] super::seccomp::SeccompError),
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

/// Put a rootful container in the node's user namespace and confine it.
///
/// Adds the `user` namespace and its id mappings, grants Docker's default
/// capabilities, and swaps `/sys` for a read-only bind of the host's:
/// the kernel only lets a user namespace mount a fresh sysfs when it also
/// owns the network namespace, and ours is created by the node beforehand.
///
/// Then it confines the container (#2402): a cgroup namespace with the
/// container's own cgroup mounted over the host's `/sys/fs/cgroup`, the
/// default seccomp filter for its capabilities, `no_new_privs`, and
/// [`MASKED_PATHS`] and [`READONLY_PATHS`].
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
    let mut mapping = vec![OciIdMapping {
        container_id: 0,
        host_id: HOST_ID_BASE,
        size: CONTAINER_ID_COUNT,
    }];
    if spec.reusable_executor {
        mapping.push(OciIdMapping {
            container_id: EXECUTOR_CONTAINER_UID,
            host_id: EXECUTOR_HOST_UID,
            size: 1,
        });
    }
    spec.linux.uid_mappings = Some(mapping.clone());
    spec.linux.gid_mappings = Some(mapping);

    let names = if spec.reusable_executor {
        &[
            "CAP_KILL",
            "CAP_SETUID",
            "CAP_SETGID",
            "CAP_SETPCAP",
            "CAP_SYS_ADMIN",
        ][..]
    } else {
        DEFAULT_CAPABILITIES
    };
    let capabilities: Vec<String> = names.iter().map(|name| name.to_string()).collect();
    spec.linux.seccomp = Some(super::seccomp::default_profile(&capabilities)?);
    spec.process.capabilities = Some(OciCapabilities {
        bounding: capabilities.clone(),
        effective: capabilities.clone(),
        permitted: capabilities,
    });

    bind_host_sys(spec);
    confine(spec);
    Ok(())
}

/// The confinement that needs no capability set: `no_new_privs`, the
/// masked and read-only paths, and a cgroup namespace with the container's
/// own cgroup over the host tree that the `/sys` bind brings in.
fn confine(spec: &mut OciSpec) {
    spec.process.no_new_privileges = true;
    spec.linux.masked_paths = MASKED_PATHS.iter().map(|p| p.to_string()).collect();
    spec.linux.readonly_paths = READONLY_PATHS.iter().map(|p| p.to_string()).collect();
    if !spec
        .linux
        .namespaces
        .iter()
        .any(|ns| ns.ns_type == "cgroup")
    {
        spec.linux.namespaces.push(OciNamespace {
            ns_type: "cgroup".to_string(),
            path: None,
        });
    }
    // A cgroup mount in a cgroup namespace shows the container's own cgroup
    // as the root, so no container sees another's names or statistics. It
    // must come after the `/sys` bind it covers: runc mounts in order.
    let cgroup = Path::new("/sys/fs/cgroup");
    if spec.mounts.iter().any(|mount| mount.destination == cgroup) {
        return;
    }
    let after_sys = spec
        .mounts
        .iter()
        .position(|mount| mount.destination == Path::new("/sys"))
        .map_or(spec.mounts.len(), |sys| sys + 1);
    spec.mounts.insert(
        after_sys,
        OciMount {
            destination: cgroup.to_path_buf(),
            source: Some(PathBuf::from("cgroup")),
            mount_type: Some("cgroup".to_string()),
            options: ["nosuid", "noexec", "nodev", "relatime", "ro"]
                .iter()
                .map(|option| option.to_string())
                .collect(),
        },
    );
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
    fn executor_delegation_is_unavailable_to_any_image_user() {
        let mut executor = spec();
        executor.reusable_executor = true;
        apply(&mut executor).unwrap();
        let mappings = executor.linux.uid_mappings.as_ref().unwrap();
        assert!(
            mappings
                .iter()
                .any(|map| map.container_id == EXECUTOR_CONTAINER_UID
                    && map.host_id == EXECUTOR_HOST_UID
                    && map.size == 1)
        );
        assert_eq!(host_id(EXECUTOR_CONTAINER_UID), None);
        assert_eq!(container_id(EXECUTOR_HOST_UID), None);
        assert!(
            executor
                .process
                .capabilities
                .unwrap()
                .effective
                .iter()
                .any(|value| value == "CAP_SYS_ADMIN")
        );
        let mut ordinary = spec();
        apply(&mut ordinary).unwrap();
        assert!(
            !ordinary
                .linux
                .uid_mappings
                .as_ref()
                .unwrap()
                .iter()
                .any(|map| map.host_id == EXECUTOR_HOST_UID)
        );
        assert!(
            !ordinary
                .process
                .capabilities
                .unwrap()
                .effective
                .iter()
                .any(|value| value == "CAP_SYS_ADMIN")
        );
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
    fn a_user_namespaced_container_is_confined_by_seccomp_and_masked_paths() {
        let mut spec = spec();
        apply(&mut spec).unwrap();
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["process"]["noNewPrivileges"], true);
        assert_eq!(json["linux"]["seccomp"]["defaultAction"], "SCMP_ACT_ERRNO");
        let masked = json["linux"]["maskedPaths"].as_array().cloned();
        assert!(
            masked
                .unwrap_or_default()
                .iter()
                .any(|path| path == "/proc/kcore"),
            "{json}"
        );
        let namespaces = json["linux"]["namespaces"].as_array().cloned();
        assert!(
            namespaces
                .unwrap_or_default()
                .iter()
                .any(|ns| ns["type"] == "cgroup"),
            "{json}"
        );
        // The host's /sys is bound recursively, so a later mount must cover
        // its cgroup tree with the container's own.
        let sys = spec
            .mounts
            .iter()
            .position(|mount| mount.destination == Path::new("/sys"))
            .unwrap();
        let cgroup = spec
            .mounts
            .iter()
            .position(|mount| mount.destination == Path::new("/sys/fs/cgroup"));
        assert!(
            cgroup.is_some_and(|cgroup| cgroup > sys),
            "{:?}",
            spec.mounts
        );
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
