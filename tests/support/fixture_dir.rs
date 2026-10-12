//! Removing a test's fixture directory, mounts and all.
//!
//! A node that ran as root mounts a small tmpfs (`reliaburger-identity`)
//! for each workload's identity beneath its volumes directory. A test that
//! fails, or a node that's killed, leaves those mounts in place, and
//! `remove_dir_all` can't remove a mount point. The cluster fixtures and the
//! OCI tests' `RootCleanup` clear their roots through [`clear_fixture_dir`].
//!
//! Included with `#[path = "support/fixture_dir.rs"] mod fixture_dir;`, or
//! re-exported by `support/cluster.rs`.

use std::path::{Path, PathBuf};

/// Remove a fixture directory and everything beneath it, so the next node
/// to use the path starts empty.
///
/// On Linux any mount beneath the directory is unmounted first: a node that
/// ran as root and was killed keeps its workload identity tmpfs
/// (`reliaburger-identity`) mounted, and `remove_dir_all` can't remove a
/// mount point. A directory that is already gone is fine. Any other failure
/// is returned, because a fixture that carried on would inherit the
/// leftover state.
pub fn clear_fixture_dir(path: &Path) -> std::io::Result<()> {
    unmount_beneath(path)?;
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn unmount_beneath(path: &Path) -> std::io::Result<()> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    for mount in mounts_beneath(&mountinfo, path) {
        let status = std::process::Command::new("umount").arg(&mount).status()?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "umount {} failed ({status})",
                mount.display()
            )));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn unmount_beneath(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// The mount points in `mountinfo` (the `/proc/self/mountinfo` format) at or
/// beneath `root`, deepest first so each can be unmounted in turn.
pub fn mounts_beneath(mountinfo: &str, root: &Path) -> Vec<PathBuf> {
    let mut mounts: Vec<PathBuf> = mountinfo
        .lines()
        // The fifth field is the mount point, with spaces, tabs, newlines
        // and backslashes written as octal escapes.
        .filter_map(|line| line.split(' ').nth(4))
        .map(|field| PathBuf::from(unescape_mount_path(field)))
        .filter(|mount| mount.starts_with(root))
        .collect();
    // A stable sort keeps mountinfo's order among mounts of equal depth.
    mounts.sort_by_key(|mount| std::cmp::Reverse(mount.components().count()));
    mounts
}

/// Undo mountinfo's `\ooo` octal escapes.
fn unescape_mount_path(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(position) = rest.find('\\') {
        out.push_str(&rest[..position]);
        let byte = rest
            .get(position + 1..position + 4)
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        match byte {
            Some(byte) => {
                out.push(char::from(byte));
                rest = &rest[position + 4..];
            }
            None => {
                out.push('\\');
                rest = &rest[position + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}
