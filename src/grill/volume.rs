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

    /// Create a managed volume directory for an app.
    ///
    /// If `size_limit` is provided and we're on Linux, creates a
    /// loop-mounted ext4 filesystem of the specified size. Otherwise
    /// creates a plain directory.
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

        std::fs::create_dir_all(&host_path)?;

        if let Some(size_str) = size_limit {
            let size_bytes = parse_resource_value(size_str)
                .map_err(|e| VolumeError::InvalidSize(format!("{size_str}: {e}")))?;

            if cfg!(target_os = "linux") && is_root() {
                self.setup_loop_mount(&host_path, size_bytes)?;
            } else if cfg!(target_os = "linux") {
                eprintln!(
                    "warning: volume size enforcement requires root; \
                     size limit {size_str} not enforced for {}",
                    host_path.display()
                );
            } else {
                eprintln!(
                    "warning: volume size enforcement requires Linux; \
                     size limit {size_str} not enforced for {}",
                    host_path.display()
                );
            }
        }

        Ok(host_path)
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
}
