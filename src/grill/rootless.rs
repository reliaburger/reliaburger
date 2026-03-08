/// Rootless OCI spec modifications (Linux only).
///
/// Adjusts an OCI runtime spec to run under runc's rootless mode.
/// Adds user namespace mappings, removes the network namespace
/// (sharing the host network for Phase 1), adjusts /sys to a
/// bind mount, and sets up cgroup paths for systemd delegation.
use super::oci::{OciIdMapping, OciMount, OciNamespace, OciSpec};
use std::path::{Path, PathBuf};

/// Modify an OCI spec for rootless runc execution.
///
/// Changes:
/// - Adds `user` namespace
/// - Removes `network` namespace (share host network, Phase 3 handles isolation)
/// - Adds UID/GID mappings (current user → container root)
/// - Adjusts `/sys` mount to `bind,ro` instead of `sysfs` (which needs privileges)
/// - Sets rootless cgroups path
/// - Resets process user to 0:0 (mapped to current user via namespace)
pub fn make_rootless(spec: &mut OciSpec, instance_name: &str) {
    // Add user namespace
    let has_user_ns = spec.linux.namespaces.iter().any(|ns| ns.ns_type == "user");
    if !has_user_ns {
        spec.linux.namespaces.push(OciNamespace {
            ns_type: "user".to_string(),
            path: None,
        });
    }

    // Remove network namespace (share host network for Phase 1)
    spec.linux.namespaces.retain(|ns| ns.ns_type != "network");

    // Add UID/GID mappings: map current user to container root (UID 0)
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();

    spec.linux.uid_mappings = Some(vec![OciIdMapping {
        container_id: 0,
        host_id: uid,
        size: 1,
    }]);

    spec.linux.gid_mappings = Some(vec![OciIdMapping {
        container_id: 0,
        host_id: gid,
        size: 1,
    }]);

    // Set process user to root inside the container (mapped to current
    // user outside via the namespace)
    spec.process.user.uid = 0;
    spec.process.user.gid = 0;

    // Adjust /sys mount: sysfs requires CAP_SYS_ADMIN outside the user
    // namespace, so bind-mount the host's /sys read-only instead
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

    // Add /dev/pts for terminal support in rootless mode
    let has_devpts = spec
        .mounts
        .iter()
        .any(|m| m.destination == Path::new("/dev/pts"));
    if !has_devpts {
        spec.mounts.push(OciMount {
            destination: PathBuf::from("/dev/pts"),
            source: Some(PathBuf::from("devpts")),
            mount_type: Some("devpts".to_string()),
            options: vec![
                "nosuid".to_string(),
                "noexec".to_string(),
                "newinstance".to_string(),
                "ptmxmode=0666".to_string(),
                "mode=0620".to_string(),
            ],
        });
    }

    // Set cgroups path for rootless cgroups v2 with systemd delegation.
    // We'll use systemd-run --user --scope to get a delegated cgroup subtree.
    spec.linux.cgroups_path = Some(format!(
        "user.slice/user-{uid}.slice/user@{uid}.service/reliaburger-{instance_name}.scope"
    ));

    // Remove resource limits — rootless runc can't set cgroup limits
    // directly. We handle this via systemd-run --user --scope instead.
    spec.linux.resources = None;
}

/// Check if the current process is running as a non-root user.
pub fn is_rootless() -> bool {
    !nix::unistd::getuid().is_root()
}

/// State directory for rootless runc.
///
/// Returns `$XDG_RUNTIME_DIR/reliaburger/runc` if available,
/// otherwise falls back to `/tmp/reliaburger-runc-{uid}`.
pub fn rootless_state_dir() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("reliaburger").join("runc")
    } else {
        let uid = nix::unistd::getuid().as_raw();
        PathBuf::from(format!("/tmp/reliaburger-runc-{uid}"))
    }
}

/// Parse a user's UID range from /etc/subuid.
///
/// Returns `(start, count)` for the first matching entry.
pub fn read_subuid_range(username: &str) -> Result<(u32, u32), std::io::Error> {
    parse_subid_file("/etc/subuid", username)
}

/// Parse a user's GID range from /etc/subgid.
///
/// Returns `(start, count)` for the first matching entry.
pub fn read_subgid_range(username: &str) -> Result<(u32, u32), std::io::Error> {
    parse_subid_file("/etc/subgid", username)
}

/// Parse a subuid/subgid file for a given username.
///
/// Format: `username:start:count` (one per line).
fn parse_subid_file(path: &str, username: &str) -> Result<(u32, u32), std::io::Error> {
    let content = std::fs::read_to_string(path)?;
    for line in content.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 3 && parts[0] == username {
            let start: u32 = parts[1].parse().map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid start id in {path}: {e}"),
                )
            })?;
            let count: u32 = parts[2].parse().map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid count in {path}: {e}"),
                )
            })?;
            return Ok((start, count));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("no entry for {username} in {path}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::oci::{
        OciLinux, OciMount, OciNamespace, OciProcess, OciResources, OciRoot, OciSpec, OciUser,
    };
    use std::path::PathBuf;

    fn sample_spec() -> OciSpec {
        OciSpec {
            root: OciRoot {
                path: "rootfs".to_string(),
                readonly: false,
            },
            process: OciProcess {
                args: vec!["sh".to_string()],
                env: vec![],
                cwd: "/".to_string(),
                user: OciUser {
                    uid: 65534,
                    gid: 65534,
                },
            },
            mounts: vec![
                OciMount {
                    destination: PathBuf::from("/proc"),
                    source: Some(PathBuf::from("proc")),
                    mount_type: Some("proc".to_string()),
                    options: vec!["nosuid".to_string()],
                },
                OciMount {
                    destination: PathBuf::from("/sys"),
                    source: Some(PathBuf::from("sysfs")),
                    mount_type: Some("sysfs".to_string()),
                    options: vec![
                        "nosuid".to_string(),
                        "noexec".to_string(),
                        "nodev".to_string(),
                        "ro".to_string(),
                    ],
                },
            ],
            linux: OciLinux {
                namespaces: vec![
                    OciNamespace {
                        ns_type: "pid".to_string(),
                        path: None,
                    },
                    OciNamespace {
                        ns_type: "network".to_string(),
                        path: None,
                    },
                    OciNamespace {
                        ns_type: "mount".to_string(),
                        path: None,
                    },
                ],
                resources: Some(OciResources {
                    cpu: None,
                    memory: None,
                }),
                cgroups_path: Some("/sys/fs/cgroup/test".to_string()),
                uid_mappings: None,
                gid_mappings: None,
            },
        }
    }

    #[test]
    fn make_rootless_adds_user_namespace() {
        let mut spec = sample_spec();
        make_rootless(&mut spec, "test-0");

        let has_user = spec.linux.namespaces.iter().any(|ns| ns.ns_type == "user");
        assert!(has_user);
    }

    #[test]
    fn make_rootless_removes_network_namespace() {
        let mut spec = sample_spec();
        make_rootless(&mut spec, "test-0");

        let has_network = spec
            .linux
            .namespaces
            .iter()
            .any(|ns| ns.ns_type == "network");
        assert!(!has_network);
    }

    #[test]
    fn make_rootless_adds_uid_gid_mappings() {
        let mut spec = sample_spec();
        make_rootless(&mut spec, "test-0");

        let uid_mappings = spec.linux.uid_mappings.as_ref().unwrap();
        assert_eq!(uid_mappings.len(), 1);
        assert_eq!(uid_mappings[0].container_id, 0);
        assert_eq!(uid_mappings[0].host_id, nix::unistd::getuid().as_raw());
        assert_eq!(uid_mappings[0].size, 1);

        let gid_mappings = spec.linux.gid_mappings.as_ref().unwrap();
        assert_eq!(gid_mappings.len(), 1);
        assert_eq!(gid_mappings[0].container_id, 0);
        assert_eq!(gid_mappings[0].host_id, nix::unistd::getgid().as_raw());
    }

    #[test]
    fn make_rootless_adjusts_sys_mount() {
        let mut spec = sample_spec();
        make_rootless(&mut spec, "test-0");

        let sys_mount = spec
            .mounts
            .iter()
            .find(|m| m.destination == PathBuf::from("/sys"))
            .unwrap();

        assert_eq!(sys_mount.mount_type, Some("none".to_string()));
        assert_eq!(sys_mount.source, Some(PathBuf::from("/sys")));
        assert!(sys_mount.options.contains(&"rbind".to_string()));
        assert!(sys_mount.options.contains(&"ro".to_string()));
    }

    #[test]
    fn make_rootless_sets_cgroups_path_for_systemd() {
        let mut spec = sample_spec();
        make_rootless(&mut spec, "test-0");

        let cgroups_path = spec.linux.cgroups_path.as_ref().unwrap();
        let uid = nix::unistd::getuid().as_raw();
        assert!(cgroups_path.contains(&format!("user-{uid}")));
        assert!(cgroups_path.contains("reliaburger-test-0.scope"));
    }

    #[test]
    fn make_rootless_removes_resources() {
        let mut spec = sample_spec();
        assert!(spec.linux.resources.is_some());

        make_rootless(&mut spec, "test-0");
        assert!(spec.linux.resources.is_none());
    }

    #[test]
    fn make_rootless_sets_user_to_root() {
        let mut spec = sample_spec();
        assert_eq!(spec.process.user.uid, 65534);

        make_rootless(&mut spec, "test-0");
        assert_eq!(spec.process.user.uid, 0);
        assert_eq!(spec.process.user.gid, 0);
    }

    #[test]
    fn is_rootless_detects_non_root() {
        // In CI and normal dev, we run as non-root
        if nix::unistd::getuid().as_raw() != 0 {
            assert!(is_rootless());
        } else {
            assert!(!is_rootless());
        }
    }

    #[test]
    fn make_rootless_idempotent() {
        let mut spec = sample_spec();
        make_rootless(&mut spec, "test-0");

        let ns_count_before = spec.linux.namespaces.len();
        make_rootless(&mut spec, "test-0");
        let ns_count_after = spec.linux.namespaces.len();

        // Should not add duplicate user namespace
        assert_eq!(ns_count_before, ns_count_after);
    }
}
