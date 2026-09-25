//! Volume management and size enforcement.
//!
//! On Linux, managed volumes with a `size` limit use a loop-mounted ext4
//! filesystem that enforces ENOSPC at the kernel level. On macOS and other
//! platforms, size limits are soft-enforced with periodic checks and warnings.

mod owned;

use std::path::{Path, PathBuf};

use crate::config::types::parse_byte_size;

/// Errors from volume operations.
#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    /// Ownership or observed storage no longer permits a destructive operation.
    #[error("test storage ownership: {0}")]
    Ownership(String),
    #[error("failed to create volume at {path}: {reason}")]
    CreateFailed { path: String, reason: String },
    #[error("invalid size value: {0}")]
    InvalidSize(String),
    #[error("mount path {0:?} escapes the volumes directory")]
    PathTraversal(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
type ProvisionFault = fn(&Path, &Path) -> Result<(), VolumeError>;

/// Manages volume creation and size enforcement.
pub struct VolumeManager {
    /// Base directory for managed volumes.
    volumes_dir: PathBuf,
    #[cfg(test)]
    before_test_provision: Option<ProvisionFault>,
}

impl VolumeManager {
    /// Create a new volume manager.
    pub fn new(volumes_dir: impl Into<PathBuf>) -> Self {
        Self {
            volumes_dir: volumes_dir.into(),
            #[cfg(test)]
            before_test_provision: None,
        }
    }

    /// Claim and provision disposable test storage before runtime creation.
    /// The caller must serialise this with deployment and lease retirement.
    pub fn prepare_test_storage(
        &self,
        namespace: &str,
        app: &str,
        spec: &crate::config::app::AppSpec,
    ) -> Result<(), VolumeError> {
        owned::prepare(self, namespace, app, spec)
    }

    /// Delete only journal-owned test storage after every workload has stopped.
    /// Ordinary application namespaces refuse before any filesystem mutation.
    pub fn retire_test_storage(&self, namespace: &str, app: &str) -> Result<(), VolumeError> {
        owned::retire(self, namespace, app)
    }

    /// Create a managed volume for an app.
    ///
    /// Backend by environment ([`super::btrfs::select_backend`]): a
    /// Btrfs subvolume when the volumes directory is on Btrfs (qgroup
    /// limit when sized), a loop-mounted ext4 filesystem for sized
    /// volumes elsewhere on Linux (root), a plain directory otherwise.
    /// The chosen backend is recorded in a sidecar file so delete and
    /// snapshot paths know what they're handling.
    ///
    /// Idempotent: instance restarts re-drive startup, so a volume
    /// whose sidecar exists is already provisioned — re-running must
    /// not stack loop mounts or fail on an existing subvolume.
    pub fn create_managed_volume(
        &self,
        namespace: &str,
        app_name: &str,
        mount_path: &Path,
        size_limit: Option<&str>,
    ) -> Result<PathBuf, VolumeError> {
        // Defence in depth (config validation also rejects this): a `..`
        // component in the mount path would escape `volumes_dir` once joined,
        // giving a deploy a read-write bind mount of an arbitrary host
        // directory as root. Refuse rather than provision.
        if mount_path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(VolumeError::PathTraversal(mount_path.display().to_string()));
        }
        let relative_path = mount_path.strip_prefix("/").unwrap_or(mount_path);
        let host_path = self
            .volumes_dir
            .join(namespace)
            .join(app_name)
            .join(relative_path);

        if let Some(backend) = self.backend_of(&host_path) {
            // A loop mount doesn't survive a reboot. Without this the app
            // would write into the bare mountpoint on the root filesystem,
            // unbounded, while its real data sat unmounted in the image.
            if backend == super::btrfs::VolumeBackend::LoopMount {
                self.remount_loop(&host_path)?;
            }
            return Ok(host_path); // already provisioned
        }

        let size_bytes = size_limit
            .map(|s| parse_byte_size(s).map_err(|e| VolumeError::InvalidSize(format!("{s}: {e}"))))
            .transpose()?;

        let backend = super::btrfs::select_backend(
            super::btrfs::is_btrfs(&self.volumes_dir),
            cfg!(target_os = "linux") && is_root(),
            size_bytes.is_some(),
        );

        // Parents are always plain directories; only the leaf differs.
        if let Some(parent) = host_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        match backend {
            super::btrfs::VolumeBackend::BtrfsSubvolume => {
                super::btrfs::create_subvolume(&host_path, size_bytes, &self.volumes_dir).map_err(
                    |reason| VolumeError::CreateFailed {
                        path: host_path.display().to_string(),
                        reason,
                    },
                )?;
            }
            super::btrfs::VolumeBackend::LoopMount => {
                std::fs::create_dir_all(&host_path)?;
                // select_backend only picks LoopMount when sized.
                if let Some(bytes) = size_bytes {
                    self.setup_loop_mount(&host_path, bytes)?;
                }
            }
            super::btrfs::VolumeBackend::Plain => {
                std::fs::create_dir_all(&host_path)?;
                if let Some(size_str) = size_limit {
                    let need = if cfg!(target_os = "linux") {
                        "root"
                    } else {
                        "Linux"
                    };
                    eprintln!(
                        "warning: volume size enforcement requires {need}; \
                         size limit {size_str} not enforced for {}",
                        host_path.display()
                    );
                }
            }
        }

        self.write_backend(&host_path, backend)?;
        Ok(host_path)
    }

    /// Container mount paths of every provisioned managed volume for
    /// an app, reconstructed from the `*.volume.json` sidecars — the
    /// filesystem is the source of truth, so this works without the
    /// app spec (snapshots of a stopped app, for instance).
    pub fn provisioned_volumes(&self, namespace: &str, app_name: &str) -> Vec<String> {
        let root = self.volumes_dir.join(namespace).join(app_name);
        let mut volumes = Vec::new();
        Self::walk_sidecars(&root, &root, &mut volumes);
        volumes.sort();
        volumes
    }

    fn walk_sidecars(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::walk_sidecars(root, &path, out);
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && let Some(volume_name) = name.strip_suffix(".volume.json")
            {
                let volume_dir = path.with_file_name(volume_name);
                if let Ok(relative) = volume_dir.strip_prefix(root) {
                    out.push(format!("/{}", relative.to_string_lossy()));
                }
            }
        }
    }

    /// `(namespace, app)` pairs with at least one provisioned managed
    /// volume — the scheduled snapshot sweep's work list.
    pub fn provisioned_apps(&self) -> Vec<(String, String)> {
        let mut apps = Vec::new();
        let Ok(namespaces) = std::fs::read_dir(&self.volumes_dir) else {
            return apps;
        };
        for ns_entry in namespaces.flatten() {
            let ns_name = ns_entry.file_name().to_string_lossy().into_owned();
            // Skip bookkeeping dirs (.snapshots, .config).
            if ns_name.starts_with('.')
                || crate::testkit::lease::valid_test_namespace(&ns_name)
                || !ns_entry.path().is_dir()
            {
                continue;
            }
            let Ok(app_dirs) = std::fs::read_dir(ns_entry.path()) else {
                continue;
            };
            for app_entry in app_dirs.flatten() {
                if !app_entry.path().is_dir() {
                    continue;
                }
                let app_name = app_entry.file_name().to_string_lossy().into_owned();
                if !self.provisioned_volumes(&ns_name, &app_name).is_empty() {
                    apps.push((ns_name.clone(), app_name));
                }
            }
        }
        apps.sort();
        apps
    }

    /// The recorded backend of a provisioned volume, if any.
    pub fn backend_of(&self, host_path: &Path) -> Option<super::btrfs::VolumeBackend> {
        read_sidecar(host_path).map(|sidecar| sidecar.backend)
    }

    fn write_backend(
        &self,
        host_path: &Path,
        backend: super::btrfs::VolumeBackend,
    ) -> Result<(), VolumeError> {
        write_sidecar(
            host_path,
            &VolumeSidecar {
                schema: 1,
                backend,
                owner: None,
            },
        )
    }

    /// Sidecar sibling of the volume directory:
    /// `.../data` → `.../data.volume.json`.
    fn sidecar_path(host_path: &Path) -> PathBuf {
        let mut name = host_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("volume"))
            .to_os_string();
        name.push(".volume.json");
        host_path.with_file_name(name)
    }

    /// Set up a loop-mounted ext4 filesystem (Linux only).
    ///
    /// Creates a sparse file, formats it with ext4, and loop-mounts it.
    /// Writes beyond the quota fail with ENOSPC.
    #[cfg(target_os = "linux")]
    fn setup_loop_mount(&self, path: &Path, size_bytes: u64) -> Result<(), VolumeError> {
        use std::process::Command;

        let img_path = path.with_extension("img");

        // Create sparse file
        let status = Command::new("fallocate")
            .args(["-l", &size_bytes.to_string()])
            .arg(&img_path)
            .status()
            .map_err(|e| VolumeError::CreateFailed {
                path: path.display().to_string(),
                reason: format!("fallocate: {e}"),
            })?;
        if !status.success() {
            return Err(VolumeError::CreateFailed {
                path: path.display().to_string(),
                reason: "fallocate failed".to_string(),
            });
        }

        // Format with ext4
        let status = Command::new("mkfs.ext4")
            .args(["-F", "-q"])
            .arg(&img_path)
            .status()
            .map_err(|e| VolumeError::CreateFailed {
                path: path.display().to_string(),
                reason: format!("mkfs.ext4: {e}"),
            })?;
        if !status.success() {
            return Err(VolumeError::CreateFailed {
                path: path.display().to_string(),
                reason: "mkfs.ext4 failed".to_string(),
            });
        }

        // Loop mount
        let status = Command::new("mount")
            .args(["-o", "loop"])
            .arg(&img_path)
            .arg(path)
            .status()
            .map_err(|e| VolumeError::CreateFailed {
                path: path.display().to_string(),
                reason: format!("mount: {e}"),
            })?;
        if !status.success() {
            return Err(VolumeError::CreateFailed {
                path: path.display().to_string(),
                reason: "loop mount failed (requires root)".to_string(),
            });
        }

        // A fresh ext4 filesystem has `lost+found`, so the app would not see
        // an empty volume. Redis's entrypoint then refuses to take its data
        // directory over and Postgres's initdb refuses to run. e2fsck makes a
        // new one if it ever needs it.
        std::fs::remove_dir(path.join("lost+found"))?;

        Ok(())
    }

    /// No-op on non-Linux platforms.
    #[cfg(not(target_os = "linux"))]
    fn setup_loop_mount(&self, _path: &Path, _size_bytes: u64) -> Result<(), VolumeError> {
        Ok(())
    }

    /// Mount a provisioned loop volume's image again if it isn't mounted.
    #[cfg(target_os = "linux")]
    fn remount_loop(&self, path: &Path) -> Result<(), VolumeError> {
        if super::rootfs::is_mountpoint(path) {
            return Ok(());
        }
        let img_path = path.with_extension("img");
        let failed = |reason: String| VolumeError::CreateFailed {
            path: path.display().to_string(),
            reason,
        };
        if !img_path.is_file() {
            return Err(failed(format!(
                "loop volume image {} is missing",
                img_path.display()
            )));
        }
        // Anything written to the bare mountpoint while the image was
        // unmounted would be hidden by the mount. Refuse rather than hide it.
        if std::fs::read_dir(path)?.next().is_some() {
            return Err(failed(
                "loop volume mountpoint holds files written while its image was unmounted; \
                 move them into the mounted volume first"
                    .to_string(),
            ));
        }
        let status = std::process::Command::new("mount")
            .args(["-o", "loop"])
            .arg(&img_path)
            .arg(path)
            .status()
            .map_err(|e| failed(format!("mount: {e}")))?;
        if !status.success() {
            return Err(failed("loop remount failed (requires root)".to_string()));
        }
        Ok(())
    }

    /// No-op on non-Linux platforms.
    #[cfg(not(target_os = "linux"))]
    fn remount_loop(&self, _path: &Path) -> Result<(), VolumeError> {
        Ok(())
    }

    /// Check the current disk usage of a volume path (in bytes).
    pub fn check_usage(path: &Path) -> Result<u64, VolumeError> {
        let mut total = 0u64;
        if path.is_dir() {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let meta = entry.metadata()?;
                if meta.is_file() {
                    total += meta.len();
                } else if meta.is_dir() {
                    total += Self::check_usage(&entry.path())?;
                }
            }
        }
        Ok(total)
    }

    /// The base volumes directory.
    pub fn volumes_dir(&self) -> &Path {
        &self.volumes_dir
    }
}

/// Sidecar metadata recorded next to each managed volume.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct VolumeSidecar {
    schema: u32,
    backend: super::btrfs::VolumeBackend,
    /// Who the volume was last handed to; `None` until its first mount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<VolumeOwner>,
}

fn read_sidecar(host_path: &Path) -> Option<VolumeSidecar> {
    let bytes = std::fs::read(VolumeManager::sidecar_path(host_path)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Replace the sidecar atomically: a torn sidecar reads as "never
/// provisioned", and provisioning again would stack loop mounts.
fn write_sidecar(host_path: &Path, sidecar: &VolumeSidecar) -> Result<(), VolumeError> {
    let json = serde_json::to_vec_pretty(sidecar).map_err(|e| VolumeError::CreateFailed {
        path: host_path.display().to_string(),
        reason: format!("sidecar serialise: {e}"),
    })?;
    let path = VolumeManager::sidecar_path(host_path);
    let staged = path.with_extension("json.tmp");
    std::fs::write(&staged, json)?;
    std::fs::rename(&staged, &path)?;
    Ok(())
}

/// A host uid and gid: who a container process runs as, or who owns a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VolumeOwner {
    pub uid: u32,
    pub gid: u32,
}

impl std::fmt::Display for VolumeOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.uid, self.gid)
    }
}

/// What happens to a managed volume's ownership before a container mounts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipPlan {
    /// Already handed to this user. Nothing changes, so whatever the
    /// container did with its files (an entrypoint handing them to a
    /// service user, say) survives the restart.
    Keep,
    /// Never handed over, or its root belongs to an id outside the
    /// container range: the whole tree becomes the container user's.
    HandOver,
    /// Handed to a different user before (the image's `USER` or the app's
    /// `run_as_user` changed): files still owned by that user, or in its
    /// group, move to the new one. Everything else stays put.
    Rehome { from: VolumeOwner },
}

/// Decide how a managed volume's ownership changes for a container user.
///
/// `recorded` is who the volume was last handed to, `root` who owns its
/// top directory now, `wanted` the container process's host ids. Only a
/// first mount, a changed user, or a root nobody in the container range
/// owns (a snapshot restored from before the first mount, a host-side
/// `chown`) walks the tree. An unchanged user costs nothing, however much
/// data the volume holds.
pub fn plan_ownership(
    recorded: Option<VolumeOwner>,
    root: VolumeOwner,
    wanted: VolumeOwner,
) -> OwnershipPlan {
    let root_mapped = super::userns::container_id(root.uid).is_some()
        && super::userns::container_id(root.gid).is_some();
    match recorded {
        _ if !root_mapped => OwnershipPlan::HandOver,
        None => OwnershipPlan::HandOver,
        Some(previous) if previous == wanted => OwnershipPlan::Keep,
        Some(previous) => OwnershipPlan::Rehome { from: previous },
    }
}

/// Mode a handed-over volume's root gets: the owner writes, others read.
/// Entrypoints that want it tighter (Postgres wants 0700) `chmod` it
/// themselves, which they can, because they own it.
const HANDED_OVER_MODE: u32 = 0o755;

/// Give a managed volume to the container user about to mount it.
///
/// Returns `Ok(None)` when `host_path` has no provisioning sidecar, i.e.
/// it isn't a volume Bun manages: host directories are never chowned.
/// The new owner is recorded only after the walk finishes, so an
/// interrupted hand-over runs again on the next start.
pub fn hand_to_container_user(
    host_path: &Path,
    wanted: VolumeOwner,
) -> Result<Option<OwnershipPlan>, VolumeError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let Some(mut sidecar) = read_sidecar(host_path) else {
        return Ok(None);
    };
    let metadata = std::fs::symlink_metadata(host_path)?;
    if !metadata.is_dir() {
        return Err(VolumeError::CreateFailed {
            path: host_path.display().to_string(),
            reason: "managed volume is not a directory".to_string(),
        });
    }
    let root = VolumeOwner {
        uid: metadata.uid(),
        gid: metadata.gid(),
    };
    let plan = plan_ownership(sidecar.owner, root, wanted);
    match plan {
        OwnershipPlan::Keep => return Ok(Some(plan)),
        OwnershipPlan::HandOver => {
            chown_tree(host_path, &|_| Some(wanted))?;
            std::fs::set_permissions(host_path, std::fs::Permissions::from_mode(HANDED_OVER_MODE))?;
        }
        OwnershipPlan::Rehome { from } => {
            chown_tree(host_path, &|current| {
                let moved = VolumeOwner {
                    uid: if current.uid == from.uid {
                        wanted.uid
                    } else {
                        current.uid
                    },
                    gid: if current.gid == from.gid {
                        wanted.gid
                    } else {
                        current.gid
                    },
                };
                (moved != current).then_some(moved)
            })?;
        }
    }
    sidecar.owner = Some(wanted);
    write_sidecar(host_path, &sidecar)?;
    Ok(Some(plan))
}

/// Change the owner of `root` and everything under it to whatever
/// `new_owner` says (`None` leaves an entry alone).
///
/// A container of the same app may still be running on this volume (a
/// rolling update), so it can swap a directory for a symlink to `/etc`
/// mid-walk. Every step therefore goes through the parent directory's
/// descriptor with "don't follow symlinks" flags: the walk can't be
/// steered out of the volume by a path it looked at a moment ago. It uses
/// an explicit stack, not recursion, so a maliciously deep tree fails
/// with an error rather than overflowing Bun's stack.
fn chown_tree(
    root: &Path,
    new_owner: &dyn Fn(VolumeOwner) -> Option<VolumeOwner>,
) -> Result<(), VolumeError> {
    use nix::dir::Dir;
    use nix::errno::Errno;
    use nix::fcntl::{AtFlags, OFlag};
    use nix::sys::stat::{Mode, SFlag, fstat, fstatat};
    use nix::unistd::{Gid, Uid, fchown, fchownat};
    use std::os::fd::AsRawFd;
    use std::rc::Rc;

    let directory_flags =
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let owner_of = |stat: &nix::sys::stat::FileStat| VolumeOwner {
        uid: stat.st_uid,
        gid: stat.st_gid,
    };
    let is_directory = |stat: &nix::sys::stat::FileStat| {
        SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFDIR
    };
    // Names under an open directory, which later `*at` calls resolve
    // against. `Rc` (a reference-counted pointer) lets every child share
    // its parent's descriptor, which closes when the last child is done.
    let children = |mut directory: Dir| -> Result<Vec<(Rc<Dir>, std::ffi::CString)>, Errno> {
        let mut names = Vec::new();
        for entry in directory.iter() {
            let name = entry?.file_name().to_owned();
            if name.as_bytes() != b"." && name.as_bytes() != b".." {
                names.push(name);
            }
        }
        let directory = Rc::new(directory);
        Ok(names
            .into_iter()
            .map(|name| (Rc::clone(&directory), name))
            .collect())
    };
    let io = |errno: Errno| VolumeError::Io(std::io::Error::from(errno));

    let top = Dir::open(root, directory_flags, Mode::empty()).map_err(io)?;
    let stat = fstat(top.as_raw_fd()).map_err(io)?;
    if let Some(owner) = new_owner(owner_of(&stat)) {
        fchown(
            top.as_raw_fd(),
            Some(Uid::from_raw(owner.uid)),
            Some(Gid::from_raw(owner.gid)),
        )
        .map_err(io)?;
    }
    let mut pending = children(top).map_err(io)?;
    while let Some((parent, name)) = pending.pop() {
        let parent_fd = Some(parent.as_raw_fd());
        let stat = match fstatat(parent_fd, name.as_c_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            // Deleted by the running container since we listed it.
            Err(Errno::ENOENT) => continue,
            Err(errno) => return Err(io(errno)),
        };
        if let Some(owner) = new_owner(owner_of(&stat)) {
            match fchownat(
                parent_fd,
                name.as_c_str(),
                Some(Uid::from_raw(owner.uid)),
                Some(Gid::from_raw(owner.gid)),
                AtFlags::AT_SYMLINK_NOFOLLOW,
            ) {
                Ok(()) | Err(Errno::ENOENT) => {}
                Err(errno) => return Err(io(errno)),
            }
        }
        if !is_directory(&stat) {
            continue;
        }
        match Dir::openat(parent_fd, name.as_c_str(), directory_flags, Mode::empty()) {
            Ok(directory) => pending.extend(children(directory).map_err(io)?),
            // Swapped for a symlink or a file, or removed: not ours to walk.
            Err(Errno::ELOOP | Errno::ENOTDIR | Errno::ENOENT) => continue,
            Err(errno) => return Err(io(errno)),
        }
    }
    Ok(())
}

/// Whether a container process running as `user` (host ids) can write
/// into a directory owned by `directory` with permission bits `mode`.
///
/// Judged from the mode bits alone (no ACLs, no supplementary groups), so
/// it's a hint for a warning, not a guarantee. Container root holds
/// `CAP_DAC_OVERRIDE`, but inside a user namespace that only reaches
/// files whose owner and group the namespace maps.
pub fn container_can_write(directory: VolumeOwner, mode: u32, user: VolumeOwner) -> bool {
    let container_root = super::userns::container_id(user.uid) == Some(0);
    let directory_mapped = super::userns::container_id(directory.uid).is_some()
        && super::userns::container_id(directory.gid).is_some();
    (container_root && directory_mapped)
        || (directory.uid == user.uid && mode & 0o200 != 0)
        || (directory.gid == user.gid && mode & 0o020 != 0)
        || mode & 0o002 != 0
}

/// Check if the current process is running as root.
fn is_root() -> bool {
    #[cfg(unix)]
    {
        nix::unistd::geteuid().is_root()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Parse a volume size string (e.g. "10Gi") into bytes.
pub fn parse_volume_size(s: &str) -> Result<u64, VolumeError> {
    parse_byte_size(s).map_err(|e| VolumeError::InvalidSize(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_volume_size_gi() {
        assert_eq!(parse_volume_size("10Gi").unwrap(), 10 * 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_volume_size_mi() {
        assert_eq!(parse_volume_size("512Mi").unwrap(), 512 * 1024 * 1024);
    }

    #[test]
    fn parse_volume_size_invalid() {
        assert!(parse_volume_size("not-a-size").is_err());
    }

    #[test]
    fn create_managed_volume_plain_directory() {
        let dir = tempfile::tempdir().unwrap();
        let vm = VolumeManager::new(dir.path());

        let path = vm
            .create_managed_volume("default", "redis", Path::new("/data"), None)
            .unwrap();

        assert!(path.exists());
        assert!(path.is_dir());
        assert!(path.ends_with("default/redis/data"));
    }

    #[test]
    fn create_managed_volume_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let vm = VolumeManager::new(dir.path());

        let result =
            vm.create_managed_volume("default", "app", Path::new("/../../../etc/cron.d"), None);
        assert!(
            matches!(result, Err(VolumeError::PathTraversal(_))),
            "a `..` mount path must be refused, not provisioned outside volumes_dir"
        );
        // The function returned before touching the filesystem: no namespace
        // subdirectory was created under the volumes directory. (Don't probe
        // the escaped path itself — `<tmp>/../../../etc/cron.d` canonicalises
        // to the host's real /etc/cron.d, which exists on Linux regardless.)
        assert!(!dir.path().join("default").exists());
    }

    #[test]
    fn create_records_backend_in_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let vm = VolumeManager::new(dir.path());

        let path = vm
            .create_managed_volume("default", "redis", Path::new("/data"), None)
            .unwrap();

        assert_eq!(
            vm.backend_of(&path),
            Some(crate::grill::btrfs::VolumeBackend::Plain)
        );
        // The sidecar lives NEXT TO the volume, not inside it — it must
        // not appear as a file in the container's mount.
        assert!(!path.join("data.volume.json").exists());
        assert!(path.with_file_name("data.volume.json").exists());
    }

    #[test]
    fn create_is_idempotent_for_provisioned_volumes() {
        // Instance restarts re-drive startup: a second create must not
        // re-provision (loop mounts would stack; subvolume create
        // would fail) and must not touch the data.
        let dir = tempfile::tempdir().unwrap();
        let vm = VolumeManager::new(dir.path());

        let path = vm
            .create_managed_volume("default", "redis", Path::new("/data"), None)
            .unwrap();
        std::fs::write(path.join("keep-me"), b"data").unwrap();

        let again = vm
            .create_managed_volume("default", "redis", Path::new("/data"), None)
            .unwrap();

        assert_eq!(path, again);
        assert_eq!(std::fs::read(path.join("keep-me")).unwrap(), b"data");
    }

    #[test]
    fn create_managed_volume_with_size_without_root() {
        assert!(
            !super::is_root(),
            "the rootless fallback test requires a non-root runner"
        );

        let dir = tempfile::tempdir().unwrap();
        let vm = VolumeManager::new(dir.path());

        // Without root (or on macOS), this creates a plain directory
        // with a warning instead of a loop mount.
        let path = vm
            .create_managed_volume("default", "redis", Path::new("/data"), Some("10Gi"))
            .unwrap();

        assert!(path.exists());
    }

    fn mapped(container: u32) -> VolumeOwner {
        let host = crate::grill::userns::host_id(container).unwrap();
        VolumeOwner {
            uid: host,
            gid: host,
        }
    }

    const HOST_ROOT: VolumeOwner = VolumeOwner { uid: 0, gid: 0 };

    #[test]
    fn a_fresh_volume_is_handed_to_the_container_user() {
        // Just provisioned: owned by the node's root, never handed over.
        assert_eq!(
            plan_ownership(None, HOST_ROOT, mapped(999)),
            OwnershipPlan::HandOver
        );
    }

    #[test]
    fn a_populated_volume_of_the_same_user_is_left_alone() {
        // Redis: handed to image root, whose entrypoint then chowned /data
        // to redis (999). Re-chowning to root on every start would fight it.
        assert_eq!(
            plan_ownership(Some(mapped(0)), mapped(999), mapped(0)),
            OwnershipPlan::Keep
        );
        assert_eq!(
            plan_ownership(Some(mapped(999)), mapped(999), mapped(999)),
            OwnershipPlan::Keep
        );
    }

    #[test]
    fn a_changed_container_user_rehomes_the_previous_users_files() {
        assert_eq!(
            plan_ownership(Some(mapped(1000)), mapped(1000), mapped(2000)),
            OwnershipPlan::Rehome { from: mapped(1000) }
        );
    }

    #[test]
    fn a_root_outside_the_container_range_is_handed_over_again() {
        // A snapshot restored from before the first mount, or a host-side
        // chown: the recorded owner no longer describes the tree.
        assert_eq!(
            plan_ownership(Some(mapped(999)), HOST_ROOT, mapped(999)),
            OwnershipPlan::HandOver
        );
        let half_mapped = VolumeOwner {
            uid: mapped(999).uid,
            gid: 0,
        };
        assert_eq!(
            plan_ownership(Some(mapped(999)), half_mapped, mapped(999)),
            OwnershipPlan::HandOver
        );
    }

    #[test]
    fn host_directories_are_never_chowned() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir(&data).unwrap();
        std::fs::set_permissions(&data, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();

        let plan = hand_to_container_user(&data, mapped(999)).unwrap();

        assert_eq!(plan, None, "no sidecar, so not a managed volume");
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&data).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn handing_over_records_the_owner_and_opens_the_root() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let vm = VolumeManager::new(dir.path());
        let path = vm
            .create_managed_volume("default", "redis", Path::new("/data"), None)
            .unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        std::fs::create_dir(path.join("nested")).unwrap();
        std::fs::write(path.join("nested/file"), b"x").unwrap();
        // Without root the only owner we can hand to is ourselves. That
        // still runs the whole hand-over, because our uid is outside the
        // container range.
        let me = std::fs::metadata(&path).unwrap();
        let me = VolumeOwner {
            uid: me.uid(),
            gid: me.gid(),
        };

        let plan = hand_to_container_user(&path, me).unwrap();

        assert_eq!(plan, Some(OwnershipPlan::HandOver));
        assert_eq!(read_sidecar(&path).unwrap().owner, Some(me));
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&path).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(std::fs::read(path.join("nested/file")).unwrap(), b"x");
        // Provisioning again (every instance start) keeps the record.
        vm.create_managed_volume("default", "redis", Path::new("/data"), None)
            .unwrap();
        assert_eq!(read_sidecar(&path).unwrap().owner, Some(me));
        assert_eq!(
            vm.backend_of(&path),
            Some(crate::grill::btrfs::VolumeBackend::Plain)
        );
    }

    #[test]
    fn container_writes_need_ownership_group_or_world_write() {
        let redis = mapped(999);
        // Host root's 0755 directory: nobody in the container can write.
        assert!(!container_can_write(HOST_ROOT, 0o755, redis));
        assert!(!container_can_write(HOST_ROOT, 0o755, mapped(0)));
        assert!(container_can_write(redis, 0o755, redis));
        assert!(!container_can_write(redis, 0o555, redis));
        assert!(container_can_write(
            VolumeOwner {
                uid: 0,
                gid: redis.gid
            },
            0o775,
            redis
        ));
        assert!(container_can_write(HOST_ROOT, 0o1777, redis));
        // Container root overrides permissions on files the namespace maps.
        assert!(container_can_write(redis, 0o700, mapped(0)));
    }

    #[test]
    fn check_usage_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let usage = VolumeManager::check_usage(dir.path()).unwrap();
        assert_eq!(usage, 0);
    }

    #[test]
    fn check_usage_with_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file1.txt"), "hello").unwrap();
        std::fs::write(dir.path().join("file2.txt"), "world!").unwrap();

        let usage = VolumeManager::check_usage(dir.path()).unwrap();
        assert_eq!(usage, 11); // 5 + 6 bytes
    }

    #[test]
    fn check_usage_nested_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("nested.txt"), "abc").unwrap();

        let usage = VolumeManager::check_usage(dir.path()).unwrap();
        assert_eq!(usage, 3);
    }

    // -- Btrfs integration (Linux, root, RELIABURGER_BTRFS_TESTS=1) ------

    #[cfg(target_os = "linux")]
    fn run_cmd(program: &str, args: &[&str]) {
        let status = std::process::Command::new(program)
            .args(args)
            .status()
            .unwrap_or_else(|e| panic!("{program} failed to run: {e}"));
        assert!(status.success(), "{program} {args:?} failed");
    }

    #[cfg(target_os = "linux")]
    fn owner_of(path: &Path) -> VolumeOwner {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path).unwrap();
        VolumeOwner {
            uid: metadata.uid(),
            gid: metadata.gid(),
        }
    }

    /// A changed container user takes over only what the previous one
    /// owned, and a symlink the container planted is never followed.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires Linux root"]
    fn rehoming_moves_only_the_previous_users_files_and_never_follows_symlinks() {
        assert!(is_root(), "chowning into the container range needs root");
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, b"host file").unwrap();
        let vm = VolumeManager::new(dir.path().join("volumes"));
        let path = vm
            .create_managed_volume("default", "app", Path::new("/data"), None)
            .unwrap();
        std::fs::create_dir(path.join("sub")).unwrap();
        std::fs::write(path.join("sub/mine"), b"x").unwrap();

        let plan = hand_to_container_user(&path, mapped(0)).unwrap();
        assert_eq!(plan, Some(OwnershipPlan::HandOver));
        assert_eq!(owner_of(&path.join("sub/mine")), mapped(0));
        // What the entrypoint would do: give a file to a service user,
        // and plant links out of the volume.
        let container_made = |name: &str, owner: VolumeOwner| {
            std::os::unix::fs::lchown(path.join(name), Some(owner.uid), Some(owner.gid)).unwrap();
        };
        std::fs::write(path.join("service"), b"y").unwrap();
        container_made(
            "service",
            VolumeOwner {
                uid: mapped(999).uid,
                gid: mapped(0).gid,
            },
        );
        std::os::unix::fs::symlink(&outside, path.join("link")).unwrap();
        container_made("link", mapped(0));
        std::os::unix::fs::symlink(dir.path(), path.join("dirlink")).unwrap();
        container_made("dirlink", mapped(0));

        assert_eq!(
            hand_to_container_user(&path, mapped(0)).unwrap(),
            Some(OwnershipPlan::Keep)
        );
        assert_eq!(owner_of(&path.join("service")).uid, mapped(999).uid);

        let plan = hand_to_container_user(&path, mapped(1000)).unwrap();
        assert_eq!(plan, Some(OwnershipPlan::Rehome { from: mapped(0) }));
        assert_eq!(owner_of(&path), mapped(1000));
        assert_eq!(owner_of(&path.join("sub/mine")), mapped(1000));
        assert_eq!(
            owner_of(&path.join("service")),
            VolumeOwner {
                uid: mapped(999).uid,
                gid: mapped(1000).gid
            },
            "the service user's file keeps its user"
        );
        assert_eq!(owner_of(&path.join("link")), mapped(1000));
        assert_eq!(owner_of(&outside), HOST_ROOT, "symlink target untouched");
        assert_eq!(
            owner_of(dir.path()),
            HOST_ROOT,
            "linked directory untouched"
        );
        assert_eq!(read_sidecar(&path).unwrap().owner, Some(mapped(1000)));
    }

    /// Btrfs snapshots carry ownership with the data, and the sidecar
    /// survives a restore, so a restored volume needs no second hand-over.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires Linux root, Btrfs tools, and RELIABURGER_BTRFS_TESTS=1"]
    fn a_restored_btrfs_volume_keeps_its_container_ownership() {
        assert!(
            std::env::var("RELIABURGER_BTRFS_TESTS").is_ok(),
            "set RELIABURGER_BTRFS_TESTS=1 after provisioning Btrfs tools and root access"
        );
        let scratch = tempfile::tempdir().unwrap();
        let img = scratch.path().join("btrfs.img");
        let mount = scratch.path().join("mnt");
        std::fs::create_dir_all(&mount).unwrap();
        run_cmd("truncate", &["-s", "1G", img.to_str().unwrap()]);
        run_cmd("mkfs.btrfs", &["-q", img.to_str().unwrap()]);
        run_cmd(
            "mount",
            &["-o", "loop", img.to_str().unwrap(), mount.to_str().unwrap()],
        );

        let body = || -> Result<(), String> {
            let vm = VolumeManager::new(&mount);
            let live = vm
                .create_managed_volume("default", "db", Path::new("/data"), None)
                .map_err(|e| format!("create: {e}"))?;
            if vm.backend_of(&live) != Some(crate::grill::btrfs::VolumeBackend::BtrfsSubvolume) {
                return Err("expected the btrfs subvolume backend".to_string());
            }
            hand_to_container_user(&live, mapped(0)).map_err(|e| e.to_string())?;
            std::fs::write(live.join("state"), b"v1").map_err(|e| e.to_string())?;
            std::os::unix::fs::lchown(live.join("state"), Some(mapped(999).uid), None)
                .map_err(|e| e.to_string())?;
            let snapshots = crate::grill::snapshot::SnapshotManager::new(&mount);
            let meta = snapshots
                .create("default", "db", "/data", None, std::time::SystemTime::now())
                .map_err(|e| format!("snapshot: {e}"))?;
            std::fs::write(live.join("state"), b"garbage").map_err(|e| e.to_string())?;
            snapshots
                .restore("default", "db", &meta.name)
                .map_err(|e| format!("restore: {e}"))?;

            let plan = hand_to_container_user(&live, mapped(0)).map_err(|e| e.to_string())?;
            if plan != Some(OwnershipPlan::Keep) {
                return Err(format!("restored volume re-planned as {plan:?}"));
            }
            if owner_of(&live) != mapped(0) || owner_of(&live.join("state")).uid != mapped(999).uid
            {
                return Err("restore lost the container ownership".to_string());
            }
            Ok(())
        };
        let result = body();

        let _ = std::process::Command::new("umount").arg(&mount).status();
        result.unwrap();
    }

    /// Roadmap (Phase 12): writing beyond a Btrfs qgroup quota fails.
    /// The test provisions its own loopback btrfs filesystem — no
    /// assumptions about the host's disks.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires Linux root with mkfs.ext4 and loop devices"]
    fn loop_volume_is_mounted_again_after_a_reboot() {
        assert!(nix::unistd::geteuid().is_root());
        let scratch = tempfile::tempdir().unwrap();
        let vm = VolumeManager::new(scratch.path());
        let path = vm
            .create_managed_volume("default", "db", Path::new("/data"), Some("16Mi"))
            .unwrap();
        assert_eq!(
            vm.backend_of(&path),
            Some(crate::grill::btrfs::VolumeBackend::LoopMount)
        );
        // Images like Redis and Postgres expect a new volume to be empty.
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
        std::fs::write(path.join("acknowledged"), b"36318").unwrap();
        // What a reboot leaves behind: the image, unmounted.
        run_cmd("umount", &[path.to_str().unwrap()]);
        assert!(!path.join("acknowledged").exists());

        let again = vm
            .create_managed_volume("default", "db", Path::new("/data"), Some("16Mi"))
            .unwrap();
        let data = std::fs::read(again.join("acknowledged"));
        let _ = std::process::Command::new("umount").arg(&again).status();
        assert_eq!(data.unwrap(), b"36318");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires Linux root, Btrfs tools, and RELIABURGER_BTRFS_TESTS=1"]
    fn btrfs_quota_blocks_writes_beyond_limit() {
        assert!(
            std::env::var("RELIABURGER_BTRFS_TESTS").is_ok(),
            "set RELIABURGER_BTRFS_TESTS=1 after provisioning Btrfs tools and root access"
        );

        let scratch = tempfile::tempdir().unwrap();
        let img = scratch.path().join("btrfs.img");
        let mount = scratch.path().join("mnt");
        std::fs::create_dir_all(&mount).unwrap();
        run_cmd("truncate", &["-s", "1G", img.to_str().unwrap()]);
        run_cmd("mkfs.btrfs", &["-q", img.to_str().unwrap()]);
        run_cmd(
            "mount",
            &["-o", "loop", img.to_str().unwrap(), mount.to_str().unwrap()],
        );

        // Body as a closure so the unmount always runs.
        let body = || -> Result<(), String> {
            let vm = VolumeManager::new(&mount);
            let path = vm
                .create_managed_volume("default", "db", Path::new("/data"), Some("10Mi"))
                .map_err(|e| format!("create: {e}"))?;
            if vm.backend_of(&path) != Some(crate::grill::btrfs::VolumeBackend::BtrfsSubvolume) {
                return Err("expected the btrfs subvolume backend".to_string());
            }

            // 11 MiB into a 10 MiB qgroup. Buffered writes can defer
            // enforcement, so the sync must also be checked.
            use std::io::Write;
            let attempt = (|| -> std::io::Result<()> {
                let mut file = std::fs::File::create(path.join("too-big"))?;
                file.write_all(&vec![7u8; 11 * 1024 * 1024])?;
                file.sync_all()
            })();
            if attempt.is_ok() {
                return Err("write beyond the quota unexpectedly succeeded".to_string());
            }
            Ok(())
        };
        let result = body();

        let _ = std::process::Command::new("umount").arg(&mount).status();
        result.unwrap();
    }
}
