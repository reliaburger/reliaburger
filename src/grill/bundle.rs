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

/// Create the instance's bundle directory: 0711, traversable but not listable.
///
/// Not 0700: a container with a user namespace runs its init as a mapped,
/// non-host-root user, and runc remounts the rootfs under this directory from
/// inside that namespace, which needs search permission on every directory
/// above it (0700 fails with `remount-private …: permission denied`). The
/// secret is the 0600 `config.json`, not the directory. An existing directory,
/// perhaps left by an older release with the default 0755, is tightened too.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn create_private_directory(bundle: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o711)
        .create(bundle)?;
    std::fs::set_permissions(bundle, std::fs::Permissions::from_mode(0o711))
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
    fn bundle_directory_is_owner_only() {
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("bundles").join("default__web-0");
        create_private_directory(&bundle).unwrap();
        assert_eq!(mode(&bundle), 0o711);
    }

    #[test]
    fn existing_bundle_directory_is_tightened() {
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("default__web-0");
        std::fs::create_dir(&bundle).unwrap();
        std::fs::set_permissions(&bundle, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_directory(&bundle).unwrap();
        assert_eq!(mode(&bundle), 0o711);
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
