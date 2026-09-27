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
    /// Disposable test storage cannot create persistent snapshot or upload owners.
    #[error("snapshots are not supported for lease-owned test storage")]
    TestStorage,
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

    /// A namespace, app, volume or snapshot name that would address
    /// something outside the app's own volumes and snapshots.
    #[error("invalid snapshot request: {0}")]
    InvalidInput(String),

    /// Several volumes hold a snapshot of this name (a multi-volume
    /// snapshot shares one timestamp); the caller must name the volume.
    #[error(
        "snapshot {name} of {namespace}/{app} exists for several volumes ({}); name the volume",
        volumes.join(", ")
    )]
    Ambiguous {
        namespace: String,
        app: String,
        name: String,
        volumes: Vec<String>,
    },

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

/// Longest custom snapshot name accepted.
pub const MAX_SNAPSHOT_NAME_LEN: usize = 128;

/// Check a snapshot name: one path component of 1 to
/// [`MAX_SNAPSHOT_NAME_LEN`] bytes from `[A-Za-z0-9._-]`, not starting
/// with `.`. The name is joined onto the snapshot directory, so a `/`,
/// `..` or absolute name would place a root-owned subvolume anywhere on
/// the filesystem.
pub fn validate_snapshot_name(name: &str) -> Result<(), SnapshotError> {
    let allowed = |byte: &u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-');
    if name.is_empty()
        || name.len() > MAX_SNAPSHOT_NAME_LEN
        || name.starts_with('.')
        || !name.bytes().all(|byte| allowed(&byte))
    {
        return Err(SnapshotError::InvalidInput(format!(
            "snapshot name {name:?} must be 1-{MAX_SNAPSHOT_NAME_LEN} characters from \
             [A-Za-z0-9._-] and must not start with '.'"
        )));
    }
    Ok(())
}

/// Check that a namespace and app are the lowercase DNS labels config
/// validation already demands, so each is exactly one normal path
/// component (no `/`, `..`, or leading `.` that could reach the
/// `.snapshots` bookkeeping directory).
fn validate_app_identity(namespace: &str, app: &str) -> Result<(), SnapshotError> {
    for (field, value) in [("namespace", namespace), ("app", app)] {
        if !crate::config::valid_workload_label(value) {
            return Err(SnapshotError::InvalidInput(format!(
                "{field} {value:?} is not a valid workload name"
            )));
        }
    }
    Ok(())
}

/// A container mount path in canonical form (`data`, `/data/` →
/// `/data`). Only plain components are allowed: `..`, `.` and an empty
/// path are refused rather than resolved.
fn normalise_volume_path(volume_path: &str) -> Result<String, SnapshotError> {
    let mut parts = Vec::new();
    for component in Path::new(volume_path).components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(part) => parts.push(part.to_string_lossy()),
            _ => {
                return Err(SnapshotError::InvalidInput(format!(
                    "volume {volume_path:?} must be a plain container mount path"
                )));
            }
        }
    }
    // `Path::components` silently drops interior `.` components, so check
    // the raw text too.
    if parts.is_empty() || volume_path.split('/').any(|part| part == ".") {
        return Err(SnapshotError::InvalidInput(format!(
            "volume {volume_path:?} must be a plain container mount path"
        )));
    }
    Ok(format!("/{}", parts.join("/")))
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

    /// Map a requested volume onto one of the app's provisioned managed
    /// volumes. The returned mount path comes from the inventory, never
    /// from the request, so a traversal like `/../../b/db/data` can't
    /// select another app's volume.
    fn resolve_volume(
        &self,
        namespace: &str,
        app: &str,
        requested: &str,
    ) -> Result<String, SnapshotError> {
        let wanted = normalise_volume_path(requested)?;
        super::volume::VolumeManager::new(&self.volumes_dir)
            .provisioned_volumes(namespace, app)
            .into_iter()
            .find(|volume| *volume == wanted)
            .ok_or_else(|| {
                SnapshotError::InvalidInput(format!(
                    "volume {requested:?} is not a managed volume of {namespace}/{app}"
                ))
            })
    }

    /// Verify the volume is snapshot-capable (a Btrfs subvolume, per
    /// its provisioning sidecar). `volume_path` must come from
    /// [`resolve_volume`](Self::resolve_volume).
    fn require_btrfs(
        &self,
        namespace: &str,
        app: &str,
        volume_path: &str,
    ) -> Result<PathBuf, SnapshotError> {
        let host_path = self.volume_host_path(namespace, app, volume_path);
        // A symlink in the volume's place would redirect the root-run
        // btrfs command somewhere else entirely.
        if std::fs::symlink_metadata(&host_path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(SnapshotError::InvalidInput(format!(
                "volume {volume_path} of {namespace}/{app} is a symlink"
            )));
        }
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

    /// The checks every mutating operation shares, before any I/O: a
    /// confined namespace and app, and no persistent snapshots of
    /// lease-owned test storage.
    fn check_mutable_app(namespace: &str, app: &str) -> Result<(), SnapshotError> {
        validate_app_identity(namespace, app)?;
        if crate::testkit::lease::valid_test_namespace(namespace) {
            return Err(SnapshotError::TestStorage);
        }
        Ok(())
    }

    /// Snapshot one of the app's volumes, or every provisioned volume
    /// when `volume` is `None` (they share one timestamp name). This is
    /// the entry point for requests: every input is validated before
    /// anything touches the disk.
    pub fn create_for_app(
        &self,
        namespace: &str,
        app: &str,
        volume: Option<&str>,
        custom_name: Option<&str>,
        now: SystemTime,
    ) -> Result<Vec<SnapshotMeta>, SnapshotError> {
        Self::check_mutable_app(namespace, app)?;
        if let Some(name) = custom_name {
            validate_snapshot_name(name)?;
        }
        let volumes = match volume {
            Some(requested) => vec![self.resolve_volume(namespace, app, requested)?],
            None => {
                let found = super::volume::VolumeManager::new(&self.volumes_dir)
                    .provisioned_volumes(namespace, app);
                if found.is_empty() {
                    return Err(SnapshotError::NoVolumes {
                        namespace: namespace.to_string(),
                        app: app.to_string(),
                    });
                }
                found
            }
        };
        volumes
            .iter()
            .map(|volume_path| self.create(namespace, app, volume_path, custom_name, now))
            .collect()
    }

    /// Snapshot one volume (read-only) under a name derived from `now`
    /// or supplied by the caller. `volume_path` must name one of the
    /// app's provisioned volumes.
    pub fn create(
        &self,
        namespace: &str,
        app: &str,
        volume_path: &str,
        custom_name: Option<&str>,
        now: SystemTime,
    ) -> Result<SnapshotMeta, SnapshotError> {
        Self::check_mutable_app(namespace, app)?;
        if let Some(name) = custom_name {
            validate_snapshot_name(name)?;
        }
        let volume_path = self.resolve_volume(namespace, app, volume_path)?;
        let volume_path = volume_path.as_str();
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
    ///
    /// A metadata file is only trusted where it sits: one naming another
    /// app, a non-canonical volume or an invalid snapshot name (or simply
    /// filed in the wrong place) is skipped, so restore, delete and upload
    /// never act on a path persisted metadata points them at.
    pub fn list(&self, namespace: &str, app: &str) -> Result<Vec<SnapshotMeta>, SnapshotError> {
        validate_app_identity(namespace, app)?;
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
                    && self.is_filed_at(&meta, namespace, app, &path)
                {
                    snapshots.push(meta);
                }
            }
        }
        snapshots.sort_by_key(|meta| std::cmp::Reverse(meta.created_at));
        Ok(snapshots)
    }

    /// Whether `meta` belongs to `namespace/app` and its own fields put its
    /// metadata file exactly at `meta_file`.
    fn is_filed_at(
        &self,
        meta: &SnapshotMeta,
        namespace: &str,
        app: &str,
        meta_file: &Path,
    ) -> bool {
        meta.namespace == namespace
            && meta.app == app
            && validate_snapshot_name(&meta.name).is_ok()
            && normalise_volume_path(&meta.volume_path).is_ok_and(|path| path == meta.volume_path)
            && Self::meta_path(&self.snapshot_path(meta)) == meta_file
    }

    /// Restore a snapshot over its live volume.
    ///
    /// Non-destructive on failure (M1): the new writable subvolume is built
    /// *alongside* the live one first, then swapped in by atomic subvolume
    /// rename — so a failed `btrfs snapshot` never leaves the app with no volume
    /// at all (the old code deleted the live subvolume before creating the
    /// replacement, so a failure there was unrecoverable).
    ///
    /// The caller must have verified the app has no running instances —
    /// restoring under a live workload corrupts both copies' semantics.
    /// `volume` picks between volumes that share the snapshot name.
    pub fn restore(
        &self,
        namespace: &str,
        app: &str,
        name: &str,
        volume: Option<&str>,
    ) -> Result<(), SnapshotError> {
        Self::check_mutable_app(namespace, app)?;
        let meta = self.find(namespace, app, name, volume)?;
        // The live target must still be one of the app's own volumes.
        let volume_path = self.resolve_volume(namespace, app, &meta.volume_path)?;
        let live = self.require_btrfs(namespace, app, &volume_path)?;
        let snapshot_path = self
            .snapshot_dir(namespace, app, &meta.volume_path)
            .join(&meta.name);

        let with_suffix = |suffix: &str| -> std::path::PathBuf {
            let mut path = live.clone();
            let name = live
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            path.set_file_name(format!("{name}{suffix}"));
            path
        };
        let staged = with_suffix(".restore-staged");
        let old = with_suffix(".restore-old");

        // Clean up any leftovers from a previously interrupted restore.
        let _ = btrfs::run_btrfs(&subvolume_delete_args(&staged));
        let _ = btrfs::run_btrfs(&subvolume_delete_args(&old));

        // 1. Build the replacement next to the live volume. On failure the live
        //    subvolume is untouched.
        btrfs::run_btrfs(&snapshot_create_args(&snapshot_path, &staged, false))
            .map_err(SnapshotError::Btrfs)?;

        // 2. Swap by atomic renames (same btrfs filesystem): move the live
        //    volume aside, then the staged one into place. If the second rename
        //    fails, put the original back so the app keeps a working volume.
        if let Err(e) = std::fs::rename(&live, &old) {
            let _ = btrfs::run_btrfs(&subvolume_delete_args(&staged));
            return Err(SnapshotError::Btrfs(e.to_string()));
        }
        if let Err(e) = std::fs::rename(&staged, &live) {
            let _ = std::fs::rename(&old, &live);
            let _ = btrfs::run_btrfs(&subvolume_delete_args(&staged));
            return Err(SnapshotError::Btrfs(e.to_string()));
        }

        // 3. Drop the old volume (best-effort — the restore already succeeded).
        let _ = btrfs::run_btrfs(&subvolume_delete_args(&old));
        Ok(())
    }

    /// Delete a snapshot and its metadata. `volume` picks between volumes
    /// that share the snapshot name; the retention sweep always passes it.
    /// The volume need not still be provisioned, so snapshots of deleted
    /// apps can age out.
    pub fn delete(
        &self,
        namespace: &str,
        app: &str,
        name: &str,
        volume: Option<&str>,
    ) -> Result<(), SnapshotError> {
        Self::check_mutable_app(namespace, app)?;
        let meta = self.find(namespace, app, name, volume)?;
        let snapshot_path = self.snapshot_path(&meta);

        btrfs::run_btrfs(&subvolume_delete_args(&snapshot_path)).map_err(SnapshotError::Btrfs)?;
        let _ = std::fs::remove_file(Self::meta_path(&snapshot_path));
        Ok(())
    }

    /// Look up one snapshot's metadata by name and, optionally, volume.
    ///
    /// A multi-volume snapshot gives every volume the same name, so a bare
    /// name matching more than one volume is refused as
    /// [`SnapshotError::Ambiguous`] rather than silently picking one.
    pub fn find(
        &self,
        namespace: &str,
        app: &str,
        name: &str,
        volume: Option<&str>,
    ) -> Result<SnapshotMeta, SnapshotError> {
        validate_app_identity(namespace, app)?;
        validate_snapshot_name(name)?;
        let volume = volume.map(normalise_volume_path).transpose()?;
        let mut matches: Vec<SnapshotMeta> = self
            .list(namespace, app)?
            .into_iter()
            .filter(|meta| meta.name == name)
            .filter(|meta| volume.as_ref().is_none_or(|v| *v == meta.volume_path))
            .collect();
        match matches.len() {
            0 => Err(SnapshotError::NotFound {
                namespace: namespace.to_string(),
                app: app.to_string(),
                name: name.to_string(),
            }),
            1 => Ok(matches.remove(0)),
            _ => {
                let mut volumes: Vec<String> =
                    matches.into_iter().map(|meta| meta.volume_path).collect();
                volumes.sort();
                Err(SnapshotError::Ambiguous {
                    namespace: namespace.to_string(),
                    app: app.to_string(),
                    name: name.to_string(),
                    volumes,
                })
            }
        }
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
        // The `uploaded` flag is the upload checkpoint. A torn or lost file
        // after a power cut would hide the snapshot from listing, so replace
        // it atomically and durably.
        crate::sesame::identity::atomic_write(&Self::meta_path(snapshot_path), &json)?;
        Ok(())
    }

    /// The on-disk path of a snapshot (for E3's tar + upload).
    pub fn snapshot_path(&self, meta: &SnapshotMeta) -> PathBuf {
        self.snapshot_dir(&meta.namespace, &meta.app, &meta.volume_path)
            .join(&meta.name)
    }

    /// `(namespace, app)` pairs that have snapshots on disk. The sweep
    /// prunes and uploads from here rather than from live volumes, so
    /// snapshots of since-deleted apps still age out and ship.
    pub fn apps(&self) -> Vec<(String, String)> {
        let mut apps = Vec::new();
        let root = self.volumes_dir.join(".snapshots");
        let Ok(namespaces) = std::fs::read_dir(&root) else {
            return apps;
        };
        for ns_entry in namespaces.flatten() {
            if !ns_entry.path().is_dir() {
                continue;
            }
            let ns_name = ns_entry.file_name().to_string_lossy().into_owned();
            let Ok(app_dirs) = std::fs::read_dir(ns_entry.path()) else {
                continue;
            };
            for app_entry in app_dirs.flatten() {
                if app_entry.path().is_dir() {
                    apps.push((
                        ns_name.clone(),
                        app_entry.file_name().to_string_lossy().into_owned(),
                    ));
                }
            }
        }
        apps.sort();
        apps
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
    fn leased_storage_refuses_snapshot_creation_and_deletion() {
        let root = tempfile::tempdir().unwrap();
        let manager = SnapshotManager::new(root.path());
        assert!(matches!(
            manager.create("rbtest-storage", "web", "/data", None, SystemTime::now()),
            Err(SnapshotError::TestStorage)
        ));
        assert!(matches!(
            manager.delete("rbtest-storage", "web", "snapshot", None),
            Err(SnapshotError::TestStorage)
        ));
        assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
    }

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

    #[cfg(unix)]
    #[test]
    fn write_meta_replaces_the_file_atomically() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("1752000000");
        let manager = SnapshotManager::new(dir.path());
        let mut meta = SnapshotMeta {
            schema: 1,
            namespace: "default".to_string(),
            app: "db".to_string(),
            volume_path: "/data".to_string(),
            name: "1752000000".to_string(),
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_752_000_000),
            size_bytes: 4096,
            uploaded: false,
        };
        manager.write_meta(&snapshot, &meta).unwrap();
        let path = SnapshotManager::meta_path(&snapshot);
        let before = std::fs::metadata(&path).unwrap().ino();
        meta.uploaded = true;
        manager.write_meta(&snapshot, &meta).unwrap();
        // A rename installs a new inode; an in-place write would keep it.
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), before);
        let back: SnapshotMeta = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(back.uploaded);
        let leftovers = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".reliaburger-")
            })
            .count();
        assert_eq!(leftovers, 0);
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
            snapshots.find("default", "db", "nope", None),
            Err(SnapshotError::NotFound { .. })
        ));
    }

    // -- input confinement (B01) -----------------------------------------

    /// Every file and directory under `root`, for "nothing was touched"
    /// assertions.
    fn tree(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    stack.push(path.clone());
                }
                out.push(path);
            }
        }
        out.sort();
        out
    }

    /// Two apps with provisioned volumes: `a/web:/data` and `b/db:/data`.
    fn two_apps() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let volumes = crate::grill::volume::VolumeManager::new(dir.path());
        for (namespace, app) in [("a", "web"), ("b", "db")] {
            volumes
                .create_managed_volume(namespace, app, Path::new("/data"), None)
                .unwrap();
        }
        dir
    }

    #[test]
    fn create_refuses_volumes_outside_the_apps_inventory() {
        let dir = two_apps();
        let before = tree(dir.path());
        let snapshots = SnapshotManager::new(dir.path());
        for volume in [
            "/../../b/db/data",
            "../../b/db/data",
            "/data/../../../b/db/data",
            "/",
            "",
            "/./data",
            "/nope",
        ] {
            let result = snapshots.create("a", "web", volume, None, SystemTime::UNIX_EPOCH);
            assert!(
                matches!(result, Err(SnapshotError::InvalidInput(_))),
                "volume {volume:?} gave {result:?}"
            );
            let result =
                snapshots.create_for_app("a", "web", Some(volume), None, SystemTime::UNIX_EPOCH);
            assert!(
                matches!(result, Err(SnapshotError::InvalidInput(_))),
                "volume {volume:?} gave {result:?}"
            );
        }
        assert_eq!(
            tree(dir.path()),
            before,
            "a refused request touched the disk"
        );
    }

    #[test]
    fn create_resolves_a_listed_volume_by_its_mount_path() {
        // The legitimate spellings still reach the volume, then fail
        // honestly on a non-btrfs tempdir once validation has passed.
        let dir = two_apps();
        let snapshots = SnapshotManager::new(dir.path());
        for volume in ["/data", "data", "/data/"] {
            let result = snapshots.create("a", "web", volume, None, SystemTime::UNIX_EPOCH);
            assert!(
                matches!(result, Err(SnapshotError::UnsupportedFilesystem { .. })),
                "volume {volume:?} gave {result:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn create_refuses_a_volume_replaced_by_a_symlink() {
        let dir = two_apps();
        let live = dir.path().join("a/web/data");
        std::fs::remove_dir(&live).unwrap();
        std::os::unix::fs::symlink(dir.path().join("b/db/data"), &live).unwrap();
        let snapshots = SnapshotManager::new(dir.path());
        let result = snapshots.create("a", "web", "/data", None, SystemTime::UNIX_EPOCH);
        assert!(
            matches!(result, Err(SnapshotError::InvalidInput(_))),
            "{result:?}"
        );
    }

    #[test]
    fn hostile_snapshot_names_are_refused_before_any_io() {
        let long = "n".repeat(MAX_SNAPSHOT_NAME_LEN + 1);
        let dir = two_apps();
        let before = tree(dir.path());
        let snapshots = SnapshotManager::new(dir.path());
        for name in [
            "/abs/path",
            "/tmp/owned",
            "a/b",
            "..",
            "../../../etc",
            ".",
            ".hidden",
            "",
            long.as_str(),
            "space name",
            "nul\0byte",
            "back\\slash",
        ] {
            for result in [
                snapshots
                    .create("a", "web", "/data", Some(name), SystemTime::UNIX_EPOCH)
                    .map(|_| ()),
                snapshots
                    .create_for_app("a", "web", None, Some(name), SystemTime::UNIX_EPOCH)
                    .map(|_| ()),
                snapshots.restore("a", "web", name, None),
                snapshots.delete("a", "web", name, None),
                snapshots.find("a", "web", name, None).map(|_| ()),
            ] {
                assert!(
                    matches!(result, Err(SnapshotError::InvalidInput(_))),
                    "name {name:?} gave {result:?}"
                );
            }
        }
        assert_eq!(
            tree(dir.path()),
            before,
            "a refused request touched the disk"
        );
    }

    #[test]
    fn snapshot_names_accept_the_documented_alphabet() {
        let longest = "n".repeat(MAX_SNAPSHOT_NAME_LEN);
        for name in ["1752000000", "pre-upgrade", "v1.2_rc-3", longest.as_str()] {
            assert!(validate_snapshot_name(name).is_ok(), "{name:?} refused");
        }
    }

    #[test]
    fn namespace_and_app_must_be_single_normal_components() {
        let dir = two_apps();
        let before = tree(dir.path());
        let snapshots = SnapshotManager::new(dir.path());
        for (namespace, app) in [
            ("..", "web"),
            ("a", ".."),
            ("a/../b", "db"),
            ("a", "web/../../b/db"),
            ("/abs", "web"),
            ("", "web"),
            ("a", ""),
            (".snapshots", "web"),
        ] {
            for result in [
                snapshots
                    .create(namespace, app, "/data", None, SystemTime::UNIX_EPOCH)
                    .map(|_| ()),
                snapshots
                    .create_for_app(namespace, app, None, None, SystemTime::UNIX_EPOCH)
                    .map(|_| ()),
                snapshots.list(namespace, app).map(|_| ()),
                snapshots.restore(namespace, app, "1", None),
                snapshots.delete(namespace, app, "1", None),
            ] {
                assert!(
                    matches!(result, Err(SnapshotError::InvalidInput(_))),
                    "{namespace:?}/{app:?} gave {result:?}"
                );
            }
        }
        assert_eq!(
            tree(dir.path()),
            before,
            "a refused request touched the disk"
        );
    }

    fn sample_meta(volume_path: &str, name: &str, secs: u64) -> SnapshotMeta {
        SnapshotMeta {
            schema: 1,
            namespace: "a".to_string(),
            app: "web".to_string(),
            volume_path: volume_path.to_string(),
            name: name.to_string(),
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            size_bytes: 0,
            uploaded: false,
        }
    }

    #[test]
    fn list_ignores_metadata_that_points_outside_its_directory() {
        // Restore, delete and upload act on what `list` returns, so a
        // metadata file claiming another app, a traversal volume or a
        // hostile name must not be listed.
        let dir = tempfile::tempdir().unwrap();
        let snapshots = SnapshotManager::new(dir.path());
        let slot = dir.path().join(".snapshots/a/web/data");
        std::fs::create_dir_all(&slot).unwrap();
        let good = sample_meta("/data", "100", 100);
        snapshots
            .write_meta(&snapshots.snapshot_path(&good), &good)
            .unwrap();

        let forged = [
            SnapshotMeta {
                app: "db".to_string(),
                ..sample_meta("/data", "200", 200)
            },
            sample_meta("/../../b/db/data", "300", 300),
            sample_meta("/data", "../../../../etc", 400),
            // Well-formed, but filed under a name that isn't its own.
            sample_meta("/data", "500", 500),
        ];
        for (file, meta) in ["200", "300", "400", "not-500"].iter().zip(&forged) {
            snapshots.write_meta(&slot.join(file), meta).unwrap();
        }

        assert_eq!(snapshots.list("a", "web").unwrap(), vec![good]);
    }

    // -- identity across volumes (B02) ------------------------------------

    /// Fake on-disk snapshots of `a/web` (plain directories stand in for
    /// subvolumes).
    fn fake_snapshots(metas: &[SnapshotMeta]) -> (tempfile::TempDir, SnapshotManager) {
        let dir = tempfile::tempdir().unwrap();
        let snapshots = SnapshotManager::new(dir.path());
        for meta in metas {
            let path = snapshots.snapshot_path(meta);
            std::fs::create_dir_all(&path).unwrap();
            snapshots.write_meta(&path, meta).unwrap();
        }
        (dir, snapshots)
    }

    #[test]
    fn a_name_shared_by_several_volumes_is_ambiguous() {
        let (dir, snapshots) = fake_snapshots(&[
            sample_meta("/wal", "100", 100),
            sample_meta("/data", "100", 100),
        ]);
        let before = tree(dir.path());
        for result in [
            snapshots.find("a", "web", "100", None).map(|_| ()),
            snapshots.restore("a", "web", "100", None),
            snapshots.delete("a", "web", "100", None),
        ] {
            match result {
                Err(SnapshotError::Ambiguous { volumes, .. }) => {
                    assert_eq!(volumes, vec!["/data".to_string(), "/wal".to_string()]);
                }
                other => panic!("expected Ambiguous, got {other:?}"),
            }
        }
        assert_eq!(tree(dir.path()), before);
    }

    #[test]
    fn naming_the_volume_selects_one_snapshot() {
        let (_dir, snapshots) = fake_snapshots(&[
            sample_meta("/data", "100", 100),
            sample_meta("/wal", "100", 300),
        ]);
        for volume in ["/wal", "wal", "/wal/"] {
            let meta = snapshots.find("a", "web", "100", Some(volume)).unwrap();
            assert_eq!(meta.volume_path, "/wal");
        }
        let meta = snapshots.find("a", "web", "100", Some("/data")).unwrap();
        assert_eq!(meta.volume_path, "/data");
        assert!(matches!(
            snapshots.find("a", "web", "100", Some("/other")),
            Err(SnapshotError::NotFound { .. })
        ));
        assert!(matches!(
            snapshots.find("a", "web", "100", Some("/../wal")),
            Err(SnapshotError::InvalidInput(_))
        ));
    }

    #[test]
    fn restore_refuses_a_snapshot_whose_volume_is_no_longer_the_apps() {
        // The snapshot's metadata is well-formed, but the app no longer
        // has a `/data` volume to restore it over.
        let (_dir, snapshots) = fake_snapshots(&[sample_meta("/data", "100", 100)]);
        assert!(matches!(
            snapshots.restore("a", "web", "100", None),
            Err(SnapshotError::InvalidInput(_))
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
    #[ignore = "requires Linux root, Btrfs tools, and RELIABURGER_BTRFS_TESTS=1"]
    fn btrfs_snapshot_restore_recovers_corrupted_data() {
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
                .restore("default", "db", &meta.name, None)
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
                .delete("default", "db", &meta.name, None)
                .map_err(|e| format!("delete: {e}"))?;
            Ok(())
        };
        let result = body();

        let _ = std::process::Command::new("umount").arg(&mount).status();
        result.unwrap();
    }

    /// B02 on real Btrfs: a multi-volume snapshot shares one name, a bare
    /// name is refused as ambiguous, and naming the volume restores or
    /// deletes exactly that volume's copy.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires Linux root, Btrfs tools, and RELIABURGER_BTRFS_TESTS=1"]
    fn btrfs_multi_volume_snapshots_are_addressed_by_volume() {
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
            let volumes = crate::grill::volume::VolumeManager::new(&mount);
            let mut live = Vec::new();
            for volume in ["/data", "/wal"] {
                let path = volumes
                    .create_managed_volume("default", "db", Path::new(volume), None)
                    .map_err(|e| format!("create volume: {e}"))?;
                std::fs::write(path.join("state"), b"v1").map_err(|e| e.to_string())?;
                live.push(path);
            }

            let snapshots = SnapshotManager::new(&mount);
            let metas = snapshots
                .create_for_app("default", "db", None, None, SystemTime::now())
                .map_err(|e| format!("snapshot: {e}"))?;
            if metas.len() != 2 || metas[0].name != metas[1].name {
                return Err(format!("expected one shared name, got {metas:?}"));
            }
            let name = metas[0].name.clone();

            for path in &live {
                std::fs::write(path.join("state"), b"garbage").map_err(|e| e.to_string())?;
            }
            match snapshots.restore("default", "db", &name, None) {
                Err(SnapshotError::Ambiguous { .. }) => {}
                other => return Err(format!("bare restore gave {other:?}")),
            }
            snapshots
                .restore("default", "db", &name, Some("/wal"))
                .map_err(|e| format!("restore: {e}"))?;
            let data = std::fs::read(live[0].join("state")).map_err(|e| e.to_string())?;
            let wal = std::fs::read(live[1].join("state")).map_err(|e| e.to_string())?;
            if data != b"garbage" || wal != b"v1" {
                return Err(format!(
                    "restore touched the wrong volume: {data:?} {wal:?}"
                ));
            }

            snapshots
                .delete("default", "db", &name, Some("/data"))
                .map_err(|e| format!("delete: {e}"))?;
            let left = snapshots.list("default", "db").map_err(|e| e.to_string())?;
            if left.len() != 1 || left[0].volume_path != "/wal" {
                return Err(format!("delete removed the wrong snapshot: {left:?}"));
            }
            Ok(())
        };
        let result = body();

        let _ = std::process::Command::new("umount").arg(&mount).status();
        result.unwrap();
    }
}
