//! Private OCI bundle files.
//!
//! A runc bundle's `config.json` carries the workload's environment after
//! `ENC[AGE:...]` values have been decrypted, so it is as sensitive as the
//! secrets themselves. These helpers keep it root-only while the instance
//! owns it and delete it when the instance retires. The rest of the bundle
//! (the private overlay's upper directory) stays, so a restart of the same
//! instance keeps its files.

use std::io;
use std::path::Path;

/// File name of the OCI runtime specification inside a bundle.
pub const SPEC_FILE: &str = "config.json";

// Only the Linux runc runtime writes bundles. The helpers stay portable so
// their tests run on every development machine.

/// Create the instance's bundle directory, and close it and the bundle base
/// above it to every local user but root and `container_group` (#2412).
///
/// A container with a user namespace runs its init as a mapped, non-host-root
/// user, and runc remounts the rootfs under this directory from inside that
/// namespace, which needs search permission on every directory above it. So
/// with a `container_group` (rootful runc passes the host gid container
/// root maps to) both are 0710 and that group's: container root passes
/// through, nobody else does. Without one (rootless runc, whose container
/// root is the directory's owner) they're 0700.
///
/// Releases before 0.3.4 made them 0711, so any local user who knew an
/// instance id could reach its writable layer, and run a set-uid file a
/// container left there as the container's host uid. An existing directory
/// is tightened too.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn create_private_directory(
    bundle: &Path,
    container_group: Option<u32>,
) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(bundle)?;
    if let Some(base) = bundle.parent() {
        close_to_other_users(base, container_group)?;
    }
    close_to_other_users(bundle, container_group)
}

/// Create `directory` if it's missing and close it to every local user but
/// its owner and `container_group`: 0710 and that group's, or 0700 with
/// none.
///
/// Container files live on the host owned by the node's shared container id
/// range, set-uid bits included (#2412). A local user who can reach one can
/// run it as that range's root, the owner of every container's files. Docker
/// protects its `userns-remap` range the same way: `/var/lib/docker` is 0710
/// root and the remapped root's group.
pub(crate) fn close_to_other_users(
    directory: &Path,
    container_group: Option<u32>,
) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    // Group first, then mode: the directory is never open to a group it
    // doesn't belong to yet.
    let mode = match container_group {
        Some(group) => {
            std::os::unix::fs::lchown(directory, None, Some(group))?;
            0o710
        }
        None => 0o700,
    };
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(mode))
}

/// Atomically replace the bundle's `config.json` with an owner-only (0600) file.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn write_spec(bundle: &Path, spec: &[u8]) -> io::Result<()> {
    crate::sesame::identity::atomic_write_mode(&bundle.join(SPEC_FILE), spec, Some(0o600))
}

/// Delete the bundle's `config.json` and any temporary a killed writer left.
///
/// Call only after runc has deleted the container. A missing bundle or file
/// is fine, so retrying a half-finished cleanup succeeds. Something other
/// than a file at that path never held a spec we wrote, so it is left alone.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn remove_spec(bundle: &Path) -> io::Result<()> {
    let path = bundle.join(SPEC_FILE);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_dir() => match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        },
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match crate::sesame::identity::remove_abandoned_atomic_writes(bundle) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    std::fs::File::open(bundle)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn bundle_directory_is_closed_to_everyone_but_root_and_container_root() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("bundles");
        let bundle = base.join("default__web-0");
        // The test's own group stands in for container root's: changing a
        // directory's group to another needs root.
        let group = nix::unistd::getgid().as_raw();
        create_private_directory(&bundle, Some(group)).unwrap();
        for path in [&base, &bundle] {
            assert_eq!(mode(path), 0o710, "{}", path.display());
            assert_eq!(std::fs::metadata(path).unwrap().gid(), group);
        }
    }

    #[test]
    fn without_a_container_group_the_bundle_is_owner_only() {
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("bundles").join("default__web-0");
        create_private_directory(&bundle, None).unwrap();
        assert_eq!(mode(&bundle), 0o700);
        assert_eq!(mode(&root.path().join("bundles")), 0o700);
    }

    #[test]
    fn existing_bundle_directory_is_tightened() {
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("default__web-0");
        std::fs::create_dir(&bundle).unwrap();
        // 0711 is what releases before 0.3.4 left.
        std::fs::set_permissions(&bundle, std::fs::Permissions::from_mode(0o711)).unwrap();
        create_private_directory(&bundle, None).unwrap();
        assert_eq!(mode(&bundle), 0o700);
    }

    #[test]
    fn spec_holding_decrypted_env_is_written_owner_only() {
        let root = tempfile::tempdir().unwrap();
        let spec = br#"{"process":{"env":["DB_PASSWORD=hunter2"]}}"#;
        write_spec(root.path(), spec).unwrap();
        let path = root.path().join(SPEC_FILE);
        assert_eq!(std::fs::read(&path).unwrap(), spec);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn rewriting_a_world_readable_spec_makes_it_owner_only() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(SPEC_FILE);
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_spec(root.path(), b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn retirement_removes_the_spec_and_abandoned_temporaries() {
        let root = tempfile::tempdir().unwrap();
        write_spec(root.path(), b"DB_PASSWORD=hunter2").unwrap();
        // A writer killed before its rename leaves a full copy behind.
        let abandoned = root.path().join(".reliaburger-abandoned");
        std::fs::write(&abandoned, b"DB_PASSWORD=hunter2").unwrap();
        let upper = root.path().join("rootfs-upper");
        std::fs::create_dir(&upper).unwrap();

        remove_spec(root.path()).unwrap();

        assert!(!root.path().join(SPEC_FILE).exists());
        assert!(!abandoned.exists());
        assert!(upper.is_dir(), "the private upper survives for a restart");
    }

    #[test]
    fn removing_a_missing_spec_or_bundle_is_fine() {
        let root = tempfile::tempdir().unwrap();
        remove_spec(root.path()).unwrap();
        remove_spec(&root.path().join("never-created")).unwrap();
    }

    #[test]
    fn removal_leaves_a_directory_in_the_spec_path_alone() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(SPEC_FILE)).unwrap();
        remove_spec(root.path()).unwrap();
        assert!(root.path().join(SPEC_FILE).is_dir());
    }
}
