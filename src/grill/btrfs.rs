//! Btrfs subvolume quotas (Phase 12, slice E).
//!
//! When the managed-volumes directory sits on Btrfs, volumes become
//! subvolumes: creation is instant, size limits are qgroup limits
//! (no sparse file, no mkfs, no loop device), and snapshots (E2) are
//! O(1) copy-on-write. Everything here is either a pure argv
//! generator, a pure decision function, or a thin Linux-only shell
//! layer — the same split as the nftables work.

use std::path::Path;

/// How a managed volume's storage is provisioned. Recorded in a
/// sidecar file next to the volume so delete/snapshot paths know what
/// they're dealing with (a loop mount must be unmounted; a subvolume
/// needs `btrfs subvolume delete`, not `rm -rf`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeBackend {
    /// Plain directory — no size enforcement.
    Plain,
    /// Sparse file + ext4 + loop mount (pre-Btrfs size enforcement).
    LoopMount,
    /// Btrfs subvolume; size (when set) enforced via qgroup limit.
    BtrfsSubvolume,
}

/// Pick the storage backend for a new managed volume.
///
/// Btrfs wins whenever the volumes directory is on Btrfs — even
/// without a size limit, because only subvolumes can be snapshotted
/// (E2). Loop mounts remain the fallback for sized volumes on other
/// filesystems; everything else is a plain directory.
pub fn select_backend(on_btrfs: bool, linux_root: bool, has_size: bool) -> VolumeBackend {
    match (linux_root, on_btrfs, has_size) {
        (true, true, _) => VolumeBackend::BtrfsSubvolume,
        (true, false, true) => VolumeBackend::LoopMount,
        _ => VolumeBackend::Plain,
    }
}

/// `BTRFS_SUPER_MAGIC` — the `statfs` filesystem type id for Btrfs.
#[cfg(target_os = "linux")]
const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;

/// Whether `path` (or its nearest existing ancestor's filesystem) is
/// Btrfs. Asks the kernel via `statfs` — the same syscall `stat -f`
/// wraps.
#[cfg(target_os = "linux")]
pub fn is_btrfs(path: &Path) -> bool {
    match nix::sys::statfs::statfs(path) {
        Ok(stats) => stats.filesystem_type().0 == BTRFS_SUPER_MAGIC,
        Err(_) => false,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn is_btrfs(_path: &Path) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Argv generation (pure — unit-tested everywhere)
// ---------------------------------------------------------------------------

/// Argv for creating a subvolume at `path`.
pub fn subvolume_create_args(path: &Path) -> Vec<String> {
    vec![
        "subvolume".to_string(),
        "create".to_string(),
        path.to_string_lossy().into_owned(),
    ]
}

/// Argv for enabling quota accounting on the filesystem holding `mount`.
/// Idempotent on the btrfs side.
pub fn quota_enable_args(mount: &Path) -> Vec<String> {
    vec![
        "quota".to_string(),
        "enable".to_string(),
        mount.to_string_lossy().into_owned(),
    ]
}

/// Argv for limiting a subvolume's referenced size to `bytes`.
pub fn qgroup_limit_args(bytes: u64, subvolume: &Path) -> Vec<String> {
    vec![
        "qgroup".to_string(),
        "limit".to_string(),
        bytes.to_string(),
        subvolume.to_string_lossy().into_owned(),
    ]
}

// ---------------------------------------------------------------------------
// Execution (Linux only)
// ---------------------------------------------------------------------------

/// Run a `btrfs` subcommand, capturing stderr into the error.
#[cfg(target_os = "linux")]
pub fn run_btrfs(args: &[String]) -> Result<(), String> {
    let output = std::process::Command::new("btrfs")
        .args(args)
        .output()
        .map_err(|e| format!("failed to run btrfs: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("btrfs {} failed: {stderr}", args.join(" ")));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn run_btrfs(args: &[String]) -> Result<(), String> {
    Err(format!("btrfs unavailable on this platform: {args:?}"))
}

/// Create a subvolume at `path`, optionally limited to `size_bytes`
/// via a qgroup. `quota_mount` is the volumes root (quota accounting
/// is per-filesystem; enabling it is idempotent).
///
/// This is synchronous (called from `spawn_blocking` in the agent's
/// volume-preparation step, alongside the loop-mount path).
#[cfg(target_os = "linux")]
pub fn create_subvolume(
    path: &Path,
    size_bytes: Option<u64>,
    quota_mount: &Path,
) -> Result<(), String> {
    run_btrfs(&subvolume_create_args(path))?;
    if let Some(bytes) = size_bytes {
        run_btrfs(&quota_enable_args(quota_mount))?;
        run_btrfs(&qgroup_limit_args(bytes, path))?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn create_subvolume(
    _path: &Path,
    _size_bytes: Option<u64>,
    _quota_mount: &Path,
) -> Result<(), String> {
    Err("btrfs subvolumes require Linux".to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // -- argv generation -------------------------------------------------

    #[test]
    fn subvolume_create_syntax() {
        let args =
            subvolume_create_args(&PathBuf::from("/var/lib/reliaburger/volumes/ns/app/data"));
        assert_eq!(
            args.join(" "),
            "subvolume create /var/lib/reliaburger/volumes/ns/app/data"
        );
    }

    #[test]
    fn quota_enable_syntax() {
        let args = quota_enable_args(&PathBuf::from("/var/lib/reliaburger/volumes"));
        assert_eq!(args.join(" "), "quota enable /var/lib/reliaburger/volumes");
    }

    #[test]
    fn qgroup_limit_syntax() {
        let args = qgroup_limit_args(10 * 1024 * 1024, &PathBuf::from("/vols/ns/app/data"));
        assert_eq!(args.join(" "), "qgroup limit 10485760 /vols/ns/app/data");
    }

    // -- backend selection -------------------------------------------------

    #[test]
    fn btrfs_wins_even_without_a_size_limit() {
        // Subvolumes are what snapshots (E2) operate on.
        assert_eq!(
            select_backend(true, true, false),
            VolumeBackend::BtrfsSubvolume
        );
        assert_eq!(
            select_backend(true, true, true),
            VolumeBackend::BtrfsSubvolume
        );
    }

    #[test]
    fn loop_mount_only_for_sized_volumes_off_btrfs() {
        // (on_btrfs, linux_root, has_size)
        assert_eq!(select_backend(false, true, true), VolumeBackend::LoopMount);
        assert_eq!(select_backend(false, true, false), VolumeBackend::Plain);
    }

    #[test]
    fn plain_without_linux_root() {
        assert_eq!(select_backend(true, false, true), VolumeBackend::Plain);
        assert_eq!(select_backend(false, false, false), VolumeBackend::Plain);
    }

    #[test]
    fn backend_serde_round_trip() {
        for backend in [
            VolumeBackend::Plain,
            VolumeBackend::LoopMount,
            VolumeBackend::BtrfsSubvolume,
        ] {
            let json = serde_json::to_string(&backend).unwrap();
            let back: VolumeBackend = serde_json::from_str(&json).unwrap();
            assert_eq!(back, backend);
        }
    }
}
