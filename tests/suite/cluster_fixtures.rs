//! The wired-node fixture refuses to inherit an earlier run's state (#721).
//!
//! A `cluster_failover` retry once ran on a data directory the previous run
//! had left behind: a workload identity tmpfs was still mounted beneath it,
//! `remove_dir_all` failed, and the fixture ignored the error. These tests
//! pin the replacement: mounts beneath the directory are found (and
//! unmounted first, on Linux), and a directory that still can't be removed
//! is an error the fixture reports instead of reusing.
use std::path::{Path, PathBuf};

use crate::cluster_support::clear_fixture_dir;
use crate::cluster_support::fixture_dir::mounts_beneath;

#[test]
fn clearing_a_missing_fixture_dir_succeeds() {
    let root = tempfile::tempdir().unwrap();
    clear_fixture_dir(&root.path().join("never-created")).unwrap();
}

#[test]
fn clearing_a_fixture_dir_removes_everything_beneath_it() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("node");
    std::fs::create_dir_all(data.join("volumes/.identity/instance")).unwrap();
    std::fs::write(data.join("volumes/.identity/instance/key.pem"), "stale").unwrap();
    clear_fixture_dir(&data).unwrap();
    assert!(!data.exists());
}

#[test]
fn a_fixture_dir_that_cannot_be_removed_is_an_error_not_reused() {
    use std::os::unix::fs::PermissionsExt;
    if nix::unistd::geteuid().is_root() {
        // Root removes a read-only directory anyway, so there is nothing to
        // refuse; the mount case is what bites root, and that is covered by
        // `mounts_beneath` plus the unmount before removal.
        eprintln!("skipped: root can remove read-only directories");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("node");
    let locked = data.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(locked.join("state"), "stale").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();

    let result = clear_fixture_dir(&data);

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err(), "an unremovable directory must be reported");
    assert!(
        locked.join("state").exists(),
        "nothing pretends the stale state is gone"
    );
}

const MOUNTINFO: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
40 22 0:35 / /tmp/rb/node rw,relatime - tmpfs tmpfs rw
41 40 0:36 / /tmp/rb/node/volumes/.identity/a rw - tmpfs reliaburger-identity rw,size=1024k
42 40 0:37 / /tmp/rb/node/volumes/.identity/with\\040space rw - tmpfs reliaburger-identity rw
43 22 0:38 / /tmp/rb/node2/volumes/.identity/b rw - tmpfs reliaburger-identity rw
";

#[test]
fn mounts_beneath_a_fixture_dir_are_listed_deepest_first() {
    let mounts = mounts_beneath(MOUNTINFO, Path::new("/tmp/rb/node"));
    assert_eq!(
        mounts,
        vec![
            PathBuf::from("/tmp/rb/node/volumes/.identity/a"),
            PathBuf::from("/tmp/rb/node/volumes/.identity/with space"),
            PathBuf::from("/tmp/rb/node"),
        ]
    );
}

#[test]
fn mounts_beneath_ignore_a_sibling_sharing_the_name_prefix() {
    let mounts = mounts_beneath(MOUNTINFO, Path::new("/tmp/rb/node2"));
    assert_eq!(
        mounts,
        vec![PathBuf::from("/tmp/rb/node2/volumes/.identity/b")]
    );
    assert!(mounts_beneath(MOUNTINFO, Path::new("/srv")).is_empty());
}
