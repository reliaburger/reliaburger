//! Volume snapshots (Phase 12, slice E).
//!
//! Btrfs-only by design: a snapshot of a subvolume is an O(1)
//! copy-on-write metadata operation. On any other backend the
//! operations return a loud [`SnapshotError::UnsupportedFilesystem`] —
//! a silently *slow* copy pretending to be a snapshot would be worse
//! than an honest error.
//!
//! Layout, next to the volumes they capture:
//!
//! ```text
//! {volumes_dir}/.snapshots/{namespace}/{app}/{volume-slug}/{name}            — read-only subvolume
//! {volumes_dir}/.snapshots/{namespace}/{app}/{volume-slug}/{name}.meta.json — metadata sidecar
//! ```

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::btrfs::{self, VolumeBackend};

/// Errors from snapshot operations.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error(
        "volume {volume} of {namespace}/{app} is not a btrfs subvolume — \
         snapshots need the volumes directory on btrfs"
    )]
    UnsupportedFilesystem {
        namespace: String,
        app: String,
        volume: String,
    },

    #[error("{namespace}/{app} has running instances — stop the app before restoring")]
    AppRunning { namespace: String, app: String },

    #[error("snapshot {name} not found for {namespace}/{app}")]
    NotFound {
        namespace: String,
        app: String,
        name: String,
    },

    #[error("no managed volumes found for {namespace}/{app}")]
    NoVolumes { namespace: String, app: String },

    #[error("btrfs: {0}")]
    Btrfs(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Metadata recorded beside each snapshot.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SnapshotMeta {
    pub schema: u32,
    pub namespace: String,
    pub app: String,
    /// The volume's mount path inside the container (e.g. `/data`).
    pub volume_path: String,
    /// Snapshot name — unix seconds, or the caller's custom name.
    pub name: String,
    pub created_at: SystemTime,
    pub size_bytes: u64,
    /// Whether the scheduled-upload pass (E3) has shipped this
    /// snapshot to the object store.
    pub uploaded: bool,
}

/// A volume mount path as a single path segment: `/data` → `data`,
/// `/var/lib` → `var-lib`.
pub fn volume_slug(volume_path: &str) -> String {
    volume_path.trim_matches('/').replace('/', "-")
}

/// Snapshot name from an injected clock (unix seconds), or the
/// caller's custom name. Time is a parameter so tests are
/// deterministic.
pub fn snapshot_name(now: SystemTime, custom: Option<&str>) -> String {
    match custom {
        Some(name) => name.to_string(),
        None => now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string(),
    }
}

/// Argv for `btrfs subvolume snapshot [-r] <src> <dest>`.
pub fn snapshot_create_args(source: &Path, dest: &Path, readonly: bool) -> Vec<String> {
    let mut args = vec!["subvolume".to_string(), "snapshot".to_string()];
    if readonly {
        args.push("-r".to_string());
    }
    args.push(source.to_string_lossy().into_owned());
    args.push(dest.to_string_lossy().into_owned());
    args
}

/// Argv for `btrfs subvolume delete <path>`.
pub fn subvolume_delete_args(path: &Path) -> Vec<String> {
    vec![
        "subvolume".to_string(),
        "delete".to_string(),
        path.to_string_lossy().into_owned(),
    ]
}

/// Creates, lists, restores, and deletes volume snapshots. The
/// running-instance guard for restore lives in the agent (it owns the
/// supervisor); this type only touches the filesystem.
pub struct SnapshotManager {
    volumes_dir: PathBuf,
}

impl SnapshotManager {
    pub fn new(volumes_dir: impl Into<PathBuf>) -> Self {
        Self {
            volumes_dir: volumes_dir.into(),
        }
    }

    /// The live host path of a managed volume (mirrors
    /// `VolumeManager::create_managed_volume`).
    fn volume_host_path(&self, namespace: &str, app: &str, volume_path: &str) -> PathBuf {
        let relative = volume_path.trim_start_matches('/');
        self.volumes_dir.join(namespace).join(app).join(relative)
    }

    fn snapshot_dir(&self, namespace: &str, app: &str, volume_path: &str) -> PathBuf {
        self.volumes_dir
            .join(".snapshots")
            .join(namespace)
            .join(app)
            .join(volume_slug(volume_path))
    }

    fn meta_path(snapshot_path: &Path) -> PathBuf {
        let mut name = snapshot_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("snapshot"))
            .to_os_string();
        name.push(".meta.json");
        snapshot_path.with_file_name(name)
    }

    /// Verify the volume is snapshot-capable (a Btrfs subvolume, per
    /// its provisioning sidecar).
    fn require_btrfs(
        &self,
        namespace: &str,
        app: &str,
        volume_path: &str,
    ) -> Result<PathBuf, SnapshotError> {
        let host_path = self.volume_host_path(namespace, app, volume_path);
        let manager = super::volume::VolumeManager::new(&self.volumes_dir);
        match manager.backend_of(&host_path) {
            Some(VolumeBackend::BtrfsSubvolume) => Ok(host_path),
            _ => Err(SnapshotError::UnsupportedFilesystem {
                namespace: namespace.to_string(),
                app: app.to_string(),
                volume: volume_path.to_string(),
            }),
        }
    }

    /// Snapshot one volume (read-only) under a name derived from `now`
    /// or supplied by the caller.
    pub fn create(
        &self,
        namespace: &str,
        app: &str,
        volume_path: &str,
        custom_name: Option<&str>,
        now: SystemTime,
    ) -> Result<SnapshotMeta, SnapshotError> {
        let live = self.require_btrfs(namespace, app, volume_path)?;

        let name = snapshot_name(now, custom_name);
        let dir = self.snapshot_dir(namespace, app, volume_path);
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join(&name);

        btrfs::run_btrfs(&snapshot_create_args(&live, &dest, true))
            .map_err(SnapshotError::Btrfs)?;

        let size_bytes = super::volume::VolumeManager::check_usage(&dest).unwrap_or(0);
        let meta = SnapshotMeta {
            schema: 1,
            namespace: namespace.to_string(),
            app: app.to_string(),
            volume_path: volume_path.to_string(),
            name,
            created_at: now,
            size_bytes,
            uploaded: false,
        };
        self.write_meta(&dest, &meta)?;
        Ok(meta)
    }

    /// All snapshots for an app, newest first.
    pub fn list(&self, namespace: &str, app: &str) -> Result<Vec<SnapshotMeta>, SnapshotError> {
        let app_dir = self
            .volumes_dir
            .join(".snapshots")
            .join(namespace)
            .join(app);
        let mut snapshots = Vec::new();
        let Ok(volumes) = std::fs::read_dir(&app_dir) else {
            return Ok(snapshots); // no snapshots yet
        };
        for volume in volumes.flatten() {
            let Ok(entries) = std::fs::read_dir(volume.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json")
                    && let Ok(bytes) = std::fs::read(&path)
                    && let Ok(meta) = serde_json::from_slice::<SnapshotMeta>(&bytes)
                {
                    snapshots.push(meta);
                }
            }
        }
        snapshots.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(snapshots)
    }

    /// Restore a snapshot over its live volume: delete the live
    /// subvolume, then take a writable snapshot of the read-only one.
    ///
    /// The caller must have verified the app has no running instances —
    /// restoring under a live workload corrupts both copies' semantics.
    pub fn restore(&self, namespace: &str, app: &str, name: &str) -> Result<(), SnapshotError> {
        let meta = self.find(namespace, app, name)?;
        let live = self.require_btrfs(namespace, app, &meta.volume_path)?;
        let snapshot_path = self
            .snapshot_dir(namespace, app, &meta.volume_path)
            .join(&meta.name);

        btrfs::run_btrfs(&subvolume_delete_args(&live)).map_err(SnapshotError::Btrfs)?;
        btrfs::run_btrfs(&snapshot_create_args(&snapshot_path, &live, false))
            .map_err(SnapshotError::Btrfs)?;
        Ok(())
    }

    /// Delete a snapshot and its metadata.
    pub fn delete(&self, namespace: &str, app: &str, name: &str) -> Result<(), SnapshotError> {
        let meta = self.find(namespace, app, name)?;
        let snapshot_path = self
            .snapshot_dir(namespace, app, &meta.volume_path)
            .join(&meta.name);

        btrfs::run_btrfs(&subvolume_delete_args(&snapshot_path)).map_err(SnapshotError::Btrfs)?;
        let _ = std::fs::remove_file(Self::meta_path(&snapshot_path));
        Ok(())
    }

    /// Look up one snapshot's metadata by name.
    pub fn find(
        &self,
        namespace: &str,
        app: &str,
        name: &str,
    ) -> Result<SnapshotMeta, SnapshotError> {
        self.list(namespace, app)?
            .into_iter()
            .find(|meta| meta.name == name)
            .ok_or_else(|| SnapshotError::NotFound {
                namespace: namespace.to_string(),
                app: app.to_string(),
                name: name.to_string(),
            })
    }

    /// Rewrite a snapshot's metadata (E3 flips `uploaded`).
    pub fn write_meta(
        &self,
        snapshot_path: &Path,
        meta: &SnapshotMeta,
    ) -> Result<(), SnapshotError> {
        let json = serde_json::to_vec_pretty(meta).map_err(|e| {
            SnapshotError::Btrfs(format!("meta serialise: {e}")) // unreachable in practice
        })?;
        std::fs::write(Self::meta_path(snapshot_path), json)?;
        Ok(())
    }

    /// The on-disk path of a snapshot (for E3's tar + upload).
    pub fn snapshot_path(&self, meta: &SnapshotMeta) -> PathBuf {
        self.snapshot_dir(&meta.namespace, &meta.app, &meta.volume_path)
            .join(&meta.name)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // -- pure helpers -----------------------------------------------------

    #[test]
    fn volume_slug_flattens_paths() {
        assert_eq!(volume_slug("/data"), "data");
        assert_eq!(volume_slug("/var/lib/db"), "var-lib-db");
    }

    #[test]
    fn snapshot_name_from_injected_clock() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_752_000_000);
        assert_eq!(snapshot_name(now, None), "1752000000");
        assert_eq!(snapshot_name(now, Some("pre-upgrade")), "pre-upgrade");
    }

    #[test]
    fn snapshot_create_syntax() {
        let args = snapshot_create_args(
            &PathBuf::from("/vols/ns/app/data"),
            &PathBuf::from("/vols/.snapshots/ns/app/data/123"),
            true,
        );
        assert_eq!(
            args.join(" "),
            "subvolume snapshot -r /vols/ns/app/data /vols/.snapshots/ns/app/data/123"
        );

        // Restore direction: writable snapshot, no -r.
        let args = snapshot_create_args(&PathBuf::from("/snap"), &PathBuf::from("/live"), false);
        assert_eq!(args.join(" "), "subvolume snapshot /snap /live");
    }

    #[test]
    fn subvolume_delete_syntax() {
        let args = subvolume_delete_args(&PathBuf::from("/vols/ns/app/data"));
        assert_eq!(args.join(" "), "subvolume delete /vols/ns/app/data");
    }

    #[test]
    fn meta_serde_round_trip() {
        let meta = SnapshotMeta {
            schema: 1,
            namespace: "default".to_string(),
            app: "db".to_string(),
            volume_path: "/data".to_string(),
            name: "1752000000".to_string(),
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_752_000_000),
            size_bytes: 4096,
            uploaded: false,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: SnapshotMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back, meta);
    }

    // -- behaviour on non-btrfs volumes ------------------------------------

    #[test]
    fn create_refuses_non_btrfs_volumes() {
        // A Plain-provisioned volume (macOS tempdir) must produce the
        // loud UnsupportedFilesystem error, not a slow fake snapshot.
        let dir = tempfile::tempdir().unwrap();
        let volumes = crate::grill::volume::VolumeManager::new(dir.path());
        volumes
            .create_managed_volume("default", "db", Path::new("/data"), None)
            .unwrap();

        let snapshots = SnapshotManager::new(dir.path());
        let result = snapshots.create("default", "db", "/data", None, SystemTime::UNIX_EPOCH);
        assert!(matches!(
            result,
            Err(SnapshotError::UnsupportedFilesystem { .. })
        ));
    }

    #[test]
    fn list_is_empty_without_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let snapshots = SnapshotManager::new(dir.path());
        assert!(snapshots.list("default", "db").unwrap().is_empty());
    }

    #[test]
    fn find_missing_snapshot_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let snapshots = SnapshotManager::new(dir.path());
        assert!(matches!(
            snapshots.find("default", "db", "nope"),
            Err(SnapshotError::NotFound { .. })
        ));
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

    /// Roadmap (Phase 12), verbatim: create snapshot, corrupt data,
    /// restore from snapshot, verify data intact.
    #[cfg(target_os = "linux")]
    #[test]
    fn snapshot_restore_recovers_corrupted_data() {
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

        let body = || -> Result<(), String> {
            let volumes = crate::grill::volume::VolumeManager::new(&mount);
            let live = volumes
                .create_managed_volume("default", "db", Path::new("/data"), None)
                .map_err(|e| format!("create volume: {e}"))?;

            std::fs::write(live.join("state"), b"v1").map_err(|e| e.to_string())?;

            let snapshots = SnapshotManager::new(&mount);
            let meta = snapshots
                .create("default", "db", "/data", None, SystemTime::now())
                .map_err(|e| format!("snapshot: {e}"))?;

            // "Corrupt" the live data.
            std::fs::write(live.join("state"), b"garbage").map_err(|e| e.to_string())?;

            snapshots
                .restore("default", "db", &meta.name)
                .map_err(|e| format!("restore: {e}"))?;

            let recovered = std::fs::read(live.join("state")).map_err(|e| e.to_string())?;
            if recovered != b"v1" {
                return Err(format!("expected v1, got {recovered:?}"));
            }

            // Listing shows the snapshot; deleting removes it.
            let listed = snapshots.list("default", "db").map_err(|e| e.to_string())?;
            if listed.len() != 1 {
                return Err(format!("expected 1 snapshot, got {}", listed.len()));
            }
            snapshots
                .delete("default", "db", &meta.name)
                .map_err(|e| format!("delete: {e}"))?;
            Ok(())
        };
        let result = body();

        let _ = std::process::Command::new("umount").arg(&mount).status();
        result.unwrap();
    }
}
