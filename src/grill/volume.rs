//! Volume management and size enforcement.
//!
//! On Linux, managed volumes with a `size` limit use a loop-mounted ext4
//! filesystem that enforces ENOSPC at the kernel level. On macOS and other
//! platforms, size limits are soft-enforced with periodic checks and warnings.

use std::path::{Path, PathBuf};

use crate::config::types::parse_resource_value;

/// Errors from volume operations.
#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    #[error("failed to create volume at {path}: {reason}")]
    CreateFailed { path: String, reason: String },
    #[error("invalid size value: {0}")]
    InvalidSize(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Manages volume creation and size enforcement.
pub struct VolumeManager {
    /// Base directory for managed volumes.
    volumes_dir: PathBuf,
}

impl VolumeManager {
    /// Create a new volume manager.
    pub fn new(volumes_dir: impl Into<PathBuf>) -> Self {
        Self {
            volumes_dir: volumes_dir.into(),
        }
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
        let relative_path = mount_path.strip_prefix("/").unwrap_or(mount_path);
        let host_path = self
            .volumes_dir
            .join(namespace)
            .join(app_name)
            .join(relative_path);

        if self.backend_of(&host_path).is_some() {
            return Ok(host_path); // already provisioned
        }

        let size_bytes = size_limit
            .map(|s| {
                parse_resource_value(s).map_err(|e| VolumeError::InvalidSize(format!("{s}: {e}")))
            })
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

    /// The recorded backend of a provisioned volume, if any.
    pub fn backend_of(&self, host_path: &Path) -> Option<super::btrfs::VolumeBackend> {
        let bytes = std::fs::read(Self::sidecar_path(host_path)).ok()?;
        serde_json::from_slice::<VolumeSidecar>(&bytes)
            .ok()
            .map(|s| s.backend)
    }

    fn write_backend(
        &self,
        host_path: &Path,
        backend: super::btrfs::VolumeBackend,
    ) -> Result<(), VolumeError> {
        let sidecar = VolumeSidecar { schema: 1, backend };
        let json = serde_json::to_vec_pretty(&sidecar).map_err(|e| VolumeError::CreateFailed {
            path: host_path.display().to_string(),
            reason: format!("sidecar serialise: {e}"),
        })?;
        std::fs::write(Self::sidecar_path(host_path), json)?;
        Ok(())
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

        Ok(())
    }

    /// No-op on non-Linux platforms.
    #[cfg(not(target_os = "linux"))]
    fn setup_loop_mount(&self, _path: &Path, _size_bytes: u64) -> Result<(), VolumeError> {
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
    parse_resource_value(s).map_err(|e| VolumeError::InvalidSize(e.to_string()))
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
        assert!(path.join("data.volume.json").exists() == false);
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
        // This test only makes sense when NOT running as root.
        // Under root (e.g. `sudo cargo test` in the dev VM),
        // the code actually attempts fallocate + mkfs + mount.
        if super::is_root() {
            eprintln!("skipping: running as root");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let vm = VolumeManager::new(dir.path());

        // Without root (or on macOS), this creates a plain directory
        // with a warning instead of a loop mount.
        let path = vm
            .create_managed_volume("default", "redis", Path::new("/data"), Some("10Gi"))
            .unwrap();

        assert!(path.exists());
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

    /// Roadmap (Phase 12): writing beyond a Btrfs qgroup quota fails.
    /// The test provisions its own loopback btrfs filesystem — no
    /// assumptions about the host's disks.
    #[cfg(target_os = "linux")]
    #[test]
    fn btrfs_quota_blocks_writes_beyond_limit() {
        if std::env::var("RELIABURGER_BTRFS_TESTS").is_err() {
            eprintln!("skipping btrfs test (set RELIABURGER_BTRFS_TESTS=1 to enable)");
            return;
        }

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
