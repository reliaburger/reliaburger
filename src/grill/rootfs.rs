//! Per-instance writable root filesystems for rootful runc workloads.
//!
//! OCI image generations are shared, content-addressed lower layers. A writable
//! workload must never receive that directory directly: two replicas would see
//! and overwrite each other's changes. We mount an overlay for each instance
//! instead, keeping its upper and work directories beside the OCI bundle.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::mount::{MntFlags, MsFlags, mount, umount2};

const ROOTFS_DIR: &str = "rootfs";
const UPPER_DIR: &str = "rootfs-upper";
const WORK_DIR: &str = "rootfs-work";
const LOWER_MARKER: &str = "rootfs-lower";

/// Errors while preparing or releasing a private runc root filesystem.
#[derive(Debug, thiserror::Error)]
pub enum RootfsError {
    #[error("rootfs path {path:?} contains a character unsupported by overlayfs options")]
    UnsupportedPath { path: PathBuf },

    #[error("failed to prepare rootfs path {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to mount private rootfs at {path:?}: {source}")]
    Mount { path: PathBuf, source: Errno },

    #[error("failed to unmount private rootfs at {path:?}: {source}")]
    Unmount { path: PathBuf, source: Errno },

    #[error("rootfs worker task failed: {0}")]
    Worker(String),
}

/// An overlay mount that is automatically released unless committed to runc.
pub struct MountedRootfs {
    mountpoint: PathBuf,
    armed: bool,
}

impl MountedRootfs {
    /// The absolute rootfs path to put in the OCI spec.
    pub fn path(&self) -> &Path {
        &self.mountpoint
    }

    /// Keep the overlay mounted after bundle preparation succeeds.
    pub fn commit(mut self) {
        self.armed = false;
    }
}

impl Drop for MountedRootfs {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = unmount_strict(&self.mountpoint)
        {
            eprintln!("warning: rootfs rollback failed: {error}");
        }
    }
}

/// Mount a private writable overlay over an immutable image generation.
///
/// An existing upper directory is reused only when its marker names the same
/// canonical lower generation. This preserves an instance's files across an
/// agent restart without carrying them into a different image generation.
pub async fn mount_private(lower: PathBuf, bundle: PathBuf) -> Result<MountedRootfs, RootfsError> {
    tokio::task::spawn_blocking(move || mount_private_sync(&lower, &bundle))
        .await
        .map_err(|error| RootfsError::Worker(error.to_string()))?
}

/// Release the overlay mounted at a bundle's rootfs path.
///
/// Cleanup first waits for a normal unmount. A final lazy detach prevents a
/// dead or wedged runc process from leaving a mount visible in the host mount
/// namespace. The upper directory remains for restart/adoption recovery.
pub async fn unmount_bundle(bundle: PathBuf) -> Result<(), RootfsError> {
    tokio::task::spawn_blocking(move || unmount_cleanup(&bundle.join(ROOTFS_DIR)))
        .await
        .map_err(|error| RootfsError::Worker(error.to_string()))?
}

/// Whether `path` is an exact mountpoint in the current mount namespace.
#[cfg(test)]
pub fn is_mountpoint(path: &Path) -> bool {
    let Ok(target) = std::fs::canonicalize(path) else {
        return false;
    };
    let Ok(contents) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };

    contents.lines().any(|line| {
        line.split_whitespace()
            .nth(4)
            .map(decode_mountinfo_path)
            .is_some_and(|candidate| candidate == target)
    })
}

fn mount_private_sync(lower: &Path, bundle: &Path) -> Result<MountedRootfs, RootfsError> {
    let lower = std::fs::canonicalize(lower).map_err(|source| RootfsError::Io {
        path: lower.to_path_buf(),
        source,
    })?;
    let bundle = absolute_path(bundle)?;
    let mountpoint = bundle.join(ROOTFS_DIR);
    let upper = bundle.join(UPPER_DIR);
    let work = bundle.join(WORK_DIR);
    let marker = bundle.join(LOWER_MARKER);

    validate_overlay_path(&lower)?;
    validate_overlay_path(&upper)?;
    validate_overlay_path(&work)?;

    // A previous clean shutdown leaves an empty mountpoint. A live mount here
    // means the old runtime did not release it, so refuse to reuse its upper
    // until it can be unmounted normally.
    unmount_strict(&mountpoint)?;

    let lower_bytes = lower.as_os_str().as_bytes();
    let same_generation = std::fs::read(&marker)
        .map(|bytes| bytes == lower_bytes)
        .unwrap_or(false);
    if !same_generation {
        remove_dir_if_present(&upper)?;
        remove_dir_if_present(&work)?;
    }

    for path in [&mountpoint, &upper, &work] {
        std::fs::create_dir_all(path).map_err(|source| RootfsError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    atomic_write(&marker, lower_bytes)?;

    let options = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(),
        upper.display(),
        work.display()
    );
    mount(
        Some(OsStr::new("overlay")),
        &mountpoint,
        Some(OsStr::new("overlay")),
        MsFlags::empty(),
        Some(options.as_str()),
    )
    .map_err(|source| RootfsError::Mount {
        path: mountpoint.clone(),
        source,
    })?;

    Ok(MountedRootfs {
        mountpoint,
        armed: true,
    })
}

fn absolute_path(path: &Path) -> Result<PathBuf, RootfsError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|source| RootfsError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn validate_overlay_path(path: &Path) -> Result<(), RootfsError> {
    if path
        .as_os_str()
        .as_bytes()
        .iter()
        .any(|byte| matches!(byte, b',' | b':' | b'\\'))
    {
        return Err(RootfsError::UnsupportedPath {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn remove_dir_if_present(path: &Path) -> Result<(), RootfsError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(RootfsError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), RootfsError> {
    use std::io::Write;

    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok::<(), std::io::Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|source| RootfsError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn unmount_strict(path: &Path) -> Result<(), RootfsError> {
    match umount2(path, MntFlags::UMOUNT_NOFOLLOW) {
        Ok(()) | Err(Errno::EINVAL) | Err(Errno::ENOENT) => Ok(()),
        Err(source) => Err(RootfsError::Unmount {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn unmount_cleanup(path: &Path) -> Result<(), RootfsError> {
    for _ in 0..50 {
        match unmount_strict(path) {
            Ok(()) => return Ok(()),
            Err(RootfsError::Unmount {
                source: Errno::EBUSY,
                ..
            }) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(error) => return Err(error),
        }
    }

    match umount2(path, MntFlags::MNT_DETACH | MntFlags::UMOUNT_NOFOLLOW) {
        Ok(()) | Err(Errno::EINVAL) | Err(Errno::ENOENT) => Ok(()),
        Err(source) => Err(RootfsError::Unmount {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
fn decode_mountinfo_path(encoded: &str) -> PathBuf {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && bytes[index + 1..index + 4].iter().all(u8::is_ascii_digit)
        {
            let value = (bytes[index + 1] - b'0') * 64
                + (bytes[index + 2] - b'0') * 8
                + (bytes[index + 3] - b'0');
            decoded.push(value);
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    PathBuf::from(OsStr::from_bytes(&decoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_options_reject_delimiter_paths() {
        let error = validate_overlay_path(Path::new("/tmp/a,b")).unwrap_err();
        assert!(matches!(error, RootfsError::UnsupportedPath { .. }));
        assert!(validate_overlay_path(Path::new("/tmp/a b")).is_ok());
    }

    #[test]
    fn mountinfo_paths_are_unescaped() {
        assert_eq!(
            decode_mountinfo_path("/tmp/a\\040b\\134c"),
            PathBuf::from("/tmp/a b\\c")
        );
    }
}
