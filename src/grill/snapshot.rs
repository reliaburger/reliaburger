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

    /// Another operation owns this app's volumes right now (a restore, or
    /// a snapshot being taken or deleted).
    #[error("volumes of {namespace}/{app} are busy with another snapshot operation; retry shortly")]
    Busy { namespace: String, app: String },

    /// A restore of this volume is in flight or was interrupted and hasn't
    /// been recovered, so snapshotting it or deleting its source must wait.
    #[error("volume {volume} of {namespace}/{app} has a restore in progress")]
    RestoreInProgress {
        namespace: String,
        app: String,
        volume: String,
    },

    /// Recovery of an interrupted restore found a state it can't settle
    /// safely. Every copy is kept for an operator to inspect.
    #[error("interrupted restore of {live} needs manual recovery: {reason}")]
    RestoreRecovery { live: PathBuf, reason: String },

    /// The snapshot inventory exists but can't be read or parsed. An
    /// unreadable inventory isn't reported as an empty one.
    #[error("cannot read snapshot inventory at {path}: {reason}")]
    Inventory { path: PathBuf, reason: String },

    #[error("btrfs: {0}")]
    Btrfs(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Version of [`SnapshotMeta`]'s on-disk format.
pub const SNAPSHOT_META_SCHEMA: u32 = 2;

/// Metadata recorded beside each snapshot.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// One receipt per object-store destination that has confirmed a
    /// complete copy of this snapshot.
    pub exports: Vec<ExportReceipt>,
}

impl SnapshotMeta {
    /// Whether `destination` has confirmed a complete archive of this snapshot.
    pub fn exported_to(&self, destination: &str) -> bool {
        self.exports
            .iter()
            .any(|receipt| receipt.destination == destination)
    }
}

/// Proof that one destination holds a complete archive of a snapshot.
///
/// Written only after the archive and its manifest are both stored, so a
/// missing receipt always means "ship it (again)".
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportReceipt {
    /// The destination's identity: its URL without credentials, query or
    /// fragment.
    pub destination: String,
    /// Object key of the archive in the destination's store (it includes
    /// the destination's own prefix).
    pub archive: String,
    /// Object key of the manifest that describes the archive.
    pub manifest: String,
    /// Hex SHA-256 of the archive bytes.
    pub sha256: String,
    /// Archive size in bytes.
    pub archive_bytes: u64,
    pub completed_at: SystemTime,
}

/// A volume mount path as one path segment, reversibly: `/data` → `data`,
/// `/var/lib` → `var%2Flib`, `/a-b` → `a-b`. Only `%` and `/` are escaped,
/// so two different mount paths never share a slug (the old `-` flattening
/// mapped `/a/b` and `/a-b` to the same directory).
pub fn volume_slug(volume_path: &str) -> String {
    let mut slug = String::with_capacity(volume_path.len());
    for character in volume_path.trim_start_matches('/').chars() {
        match character {
            '%' => slug.push_str("%25"),
            '/' => slug.push_str("%2F"),
            other => slug.push(other),
        }
    }
    slug
}

/// Journal of an in-flight restore, beside the live volume:
/// `.../data` → `.../data.restore.json`.
pub fn restore_journal_path(live: &Path) -> PathBuf {
    sibling_with_suffix(live, ".restore.json")
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("volume"))
        .to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// The restore step a journal records, written *before* the step starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestorePhase {
    /// Building the staged copy; the live volume hasn't been touched.
    Staging,
    /// The staged copy is complete and is being renamed into place.
    Swapping,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreJournal {
    schema: u32,
    snapshot: String,
    phase: RestorePhase,
}

/// What recovery must do to leave exactly one complete volume at the live
/// name, given the journal's phase and which copies exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreRecovery {
    /// Nothing to do.
    Clean,
    /// The live volume was never displaced: delete the staged copy.
    DiscardStaged,
    /// The original was moved aside and the complete staged copy wasn't
    /// yet renamed into place: finish the swap, then drop the original.
    FinishSwap,
    /// The swap finished: drop the displaced original if it's still there.
    DropOld,
    /// Only the displaced original survives: put it back.
    ReinstateOld,
    /// No safe resolution; keep every copy and the journal.
    Ambiguous(&'static str),
}

/// Decide how to recover a volume from its journal phase (`None` = no
/// journal) and which of the live, staged and displaced copies exist. Pure.
pub fn plan_restore_recovery(
    phase: Option<RestorePhase>,
    live: bool,
    staged: bool,
    old: bool,
) -> RestoreRecovery {
    use RestoreRecovery::*;
    match (phase, live, staged, old) {
        (None, _, false, false) => Clean,
        (None, _, _, _) => Ambiguous("restore copies exist without a journal"),
        (Some(RestorePhase::Staging), true, _, false) => DiscardStaged,
        (Some(RestorePhase::Staging), _, _, _) => {
            Ambiguous("the live volume moved before its replacement was complete")
        }
        (Some(RestorePhase::Swapping), true, true, false) => DiscardStaged,
        (Some(RestorePhase::Swapping), false, true, true) => FinishSwap,
        (Some(RestorePhase::Swapping), true, false, _) => DropOld,
        (Some(RestorePhase::Swapping), false, false, true) => ReinstateOld,
        (Some(RestorePhase::Swapping), false, false, false) => {
            Ambiguous("no copy of the volume remains")
        }
        (Some(RestorePhase::Swapping), true, true, true) => {
            Ambiguous("live, staged and displaced copies all exist")
        }
        (Some(RestorePhase::Swapping), false, true, false) => {
            Ambiguous("only the staged copy exists; the original is missing")
        }
    }
}

/// Crash points inside a restore. Tests stop the operation dead at one,
/// with no cleanup, as if the owner had been killed there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreCrash {
    StagingJournalWritten,
    StagedCopyBuilt,
    SwappingJournalWritten,
    LiveMovedAside,
    StagedMovedIn,
    OldDropped,
}

/// Durably record a restore's phase before the step it names.
fn write_journal(live: &Path, journal: &RestoreJournal) -> Result<(), SnapshotError> {
    let json = serde_json::to_vec_pretty(journal)
        .map_err(|error| SnapshotError::Btrfs(format!("journal serialise: {error}")))?;
    crate::sesame::identity::atomic_write(&restore_journal_path(live), &json)?;
    Ok(())
}

/// Drop a settled restore's journal, durably.
fn remove_journal(live: &Path) -> std::io::Result<()> {
    std::fs::remove_file(restore_journal_path(live))?;
    sync_directory(live.parent().unwrap_or(Path::new(".")))
}

fn delete_subvolume(path: &Path) -> Result<(), String> {
    btrfs::run_btrfs(&subvolume_delete_args(path))
}

/// Every restore journal under `dir`, without descending into volumes (a
/// journal only ever sits beside one).
fn collect_journals(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files = Vec::new();
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_file() {
            files.push(path);
        } else if file_type.is_dir() {
            subdirs.push(path);
        }
    }
    let name_of = |path: &Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
    };
    let is_volume = |dir: &Path| {
        name_of(dir).is_some_and(|name| {
            files.iter().any(|file| {
                name_of(file).is_some_and(|file| {
                    file == format!("{name}.volume.json") || file == format!("{name}.restore.json")
                })
            }) || name.ends_with(".restore-staged")
                || name.ends_with(".restore-old")
        })
    };
    for subdir in &subdirs {
        if !is_volume(subdir) {
            collect_journals(subdir, out);
        }
    }
    out.extend(
        files
            .into_iter()
            .filter(|file| name_of(file).is_some_and(|name| name.ends_with(".restore.json"))),
    );
}

/// Flush a directory's entries (renames, creations) to disk.
fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
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
    #[cfg(test)]
    crash_at: Option<RestoreCrash>,
}

impl SnapshotManager {
    pub fn new(volumes_dir: impl Into<PathBuf>) -> Self {
        Self {
            volumes_dir: volumes_dir.into(),
            #[cfg(test)]
            crash_at: None,
        }
    }

    /// Stop a restore dead at `point` (tests only).
    #[cfg(test)]
    fn crash_point(&self, point: RestoreCrash) -> Result<(), SnapshotError> {
        if self.crash_at == Some(point) {
            return Err(SnapshotError::Btrfs(format!("injected crash {point:?}")));
        }
        Ok(())
    }

    #[cfg(not(test))]
    #[inline]
    fn crash_point(&self, _point: RestoreCrash) -> Result<(), SnapshotError> {
        Ok(())
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
        sibling_with_suffix(snapshot_path, ".meta.json")
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
        // Mid-restore, the live name may be empty or about to change.
        if restore_journal_path(&live).exists() {
            return Err(SnapshotError::RestoreInProgress {
                namespace: namespace.to_string(),
                app: app.to_string(),
                volume: volume_path.to_string(),
            });
        }

        let name = snapshot_name(now, custom_name);
        let dir = self.snapshot_dir(namespace, app, volume_path);
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join(&name);

        btrfs::run_btrfs(&snapshot_create_args(&live, &dest, true))
            .map_err(SnapshotError::Btrfs)?;

        let size_bytes = super::volume::VolumeManager::check_usage(&dest).unwrap_or(0);
        let meta = SnapshotMeta {
            schema: SNAPSHOT_META_SCHEMA,
            namespace: namespace.to_string(),
            app: app.to_string(),
            volume_path: volume_path.to_string(),
            name,
            created_at: now,
            size_bytes,
            exports: Vec::new(),
        };
        self.write_meta(&dest, &meta)?;
        Ok(meta)
    }

    /// All snapshots for an app, newest first. An app with no snapshot
    /// directory has none; any other failure to read the inventory (an
    /// unreadable directory, a truncated or malformed `meta.json`) is an
    /// [`SnapshotError::Inventory`] error, never a shorter list.
    ///
    /// A well-formed metadata file is only trusted where it sits: one naming
    /// another app, a non-canonical volume or an invalid snapshot name (or
    /// simply filed in the wrong place) is skipped, so restore, delete and
    /// upload never act on a path persisted metadata points them at.
    pub fn list(&self, namespace: &str, app: &str) -> Result<Vec<SnapshotMeta>, SnapshotError> {
        validate_app_identity(namespace, app)?;
        let app_dir = self
            .volumes_dir
            .join(".snapshots")
            .join(namespace)
            .join(app);
        let unreadable = |path: &Path, error: &dyn std::fmt::Display| SnapshotError::Inventory {
            path: path.to_path_buf(),
            reason: error.to_string(),
        };
        let volumes = match std::fs::read_dir(&app_dir) {
            Ok(volumes) => volumes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(unreadable(&app_dir, &error)),
        };
        let mut snapshots = Vec::new();
        for volume in volumes {
            let volume = volume.map_err(|error| unreadable(&app_dir, &error))?;
            let volume_dir = volume.path();
            let entries =
                std::fs::read_dir(&volume_dir).map_err(|error| unreadable(&volume_dir, &error))?;
            for entry in entries {
                let entry = entry.map_err(|error| unreadable(&volume_dir, &error))?;
                if let Some(meta) = self.read_listed_meta(namespace, app, &entry.path())? {
                    snapshots.push(meta);
                }
            }
        }
        snapshots.sort_by_key(|meta| std::cmp::Reverse(meta.created_at));
        Ok(snapshots)
    }

    /// Parse one entry of a volume's snapshot directory: `Some` for a
    /// metadata file filed where it belongs, `None` for anything that isn't
    /// metadata (a snapshot subvolume, an atomic-write temporary) or is
    /// filed somewhere else.
    fn read_listed_meta(
        &self,
        namespace: &str,
        app: &str,
        path: &Path,
    ) -> Result<Option<SnapshotMeta>, SnapshotError> {
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            return Ok(None);
        };
        if file_name.starts_with('.') || !file_name.ends_with(".meta.json") {
            return Ok(None);
        }
        let unreadable = |reason: String| SnapshotError::Inventory {
            path: path.to_path_buf(),
            reason,
        };
        let bytes = std::fs::read(path).map_err(|error| unreadable(error.to_string()))?;
        let meta: SnapshotMeta =
            serde_json::from_slice(&bytes).map_err(|error| unreadable(error.to_string()))?;
        if meta.schema != SNAPSHOT_META_SCHEMA {
            return Err(unreadable(format!("unsupported schema {}", meta.schema)));
        }
        Ok(self
            .is_filed_at(&meta, namespace, app, path)
            .then_some(meta))
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
    /// Crash-atomic: a journal beside the live volume records each phase
    /// *before* it starts. The replacement is built alongside the live
    /// subvolume, given the original's quota, then swapped in by two
    /// renames. If the owner dies anywhere in between,
    /// [`recover_restores`](Self::recover_restores) (run at startup, and at
    /// the start of every restore) finishes or rolls back the swap so the
    /// live name holds one complete volume, and keeps every copy when the
    /// state is ambiguous.
    ///
    /// The caller must have verified the app has no running instances and
    /// must hold the app's volumes for the whole call. `volume` picks
    /// between volumes that share the snapshot name.
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
        let snapshot_path = self.snapshot_path(&meta);
        let parent = live
            .parent()
            .ok_or_else(|| {
                SnapshotError::InvalidInput(format!("volume {volume_path} has no parent"))
            })?
            .to_path_buf();
        let staged = sibling_with_suffix(&live, ".restore-staged");
        let old = sibling_with_suffix(&live, ".restore-old");

        // Settle an earlier interruption first. Blind cleanup here used to
        // delete `.restore-old`, which after a crash between the renames is
        // the only copy of the original data.
        self.recover_volume(&live)?;

        let journal = |phase| RestoreJournal {
            schema: 1,
            snapshot: meta.name.clone(),
            phase,
        };
        write_journal(&live, &journal(RestorePhase::Staging))?;
        self.crash_point(RestoreCrash::StagingJournalWritten)?;

        // 1. Build the replacement next to the live volume. On failure the
        //    live subvolume is untouched.
        let quota = super::volume::VolumeManager::new(&self.volumes_dir).quota_of(&live);
        let built = btrfs::run_btrfs(&snapshot_create_args(&snapshot_path, &staged, false))
            .and_then(|()| match quota {
                Some(bytes) => btrfs::run_btrfs(&btrfs::qgroup_limit_args(bytes, &staged)),
                None => Ok(()),
            })
            .and_then(|()| sync_directory(&parent).map_err(|error| error.to_string()));
        if let Err(error) = built {
            return Err(self.abandon_restore(&live, SnapshotError::Btrfs(error)));
        }
        self.crash_point(RestoreCrash::StagedCopyBuilt)?;
        write_journal(&live, &journal(RestorePhase::Swapping))?;
        self.crash_point(RestoreCrash::SwappingJournalWritten)?;

        // 2. Swap by atomic renames (same btrfs filesystem): move the live
        //    volume aside, then the staged one into place.
        if let Err(error) = std::fs::rename(&live, &old) {
            return Err(self.abandon_restore(&live, error.into()));
        }
        sync_directory(&parent)?;
        self.crash_point(RestoreCrash::LiveMovedAside)?;
        if let Err(error) = std::fs::rename(&staged, &live) {
            // Put the original back. If even that fails, the journal stays
            // and recovery finishes the swap from the complete staged copy.
            std::fs::rename(&old, &live).map_err(|rollback| SnapshotError::RestoreRecovery {
                live: live.clone(),
                reason: format!("swap failed ({error}) and so did the rollback ({rollback})"),
            })?;
            sync_directory(&parent)?;
            return Err(self.abandon_restore(&live, error.into()));
        }
        sync_directory(&parent)?;
        self.crash_point(RestoreCrash::StagedMovedIn)?;

        // 3. Drop the displaced original. The restore itself has succeeded,
        //    but a copy we couldn't delete is reported, and the journal stays
        //    so the next recovery tries again.
        delete_subvolume(&old).map_err(|error| SnapshotError::RestoreRecovery {
            live: live.clone(),
            reason: format!(
                "restored, but the displaced original at {} could not be removed: {error}",
                old.display()
            ),
        })?;
        sync_directory(&parent)?;
        self.crash_point(RestoreCrash::OldDropped)?;
        remove_journal(&live)?;
        Ok(())
    }

    /// Undo a restore that failed before the live volume was replaced:
    /// drop the staged copy and the journal. Returns `cause`, extended with
    /// any cleanup failure (the journal then stays for recovery).
    fn abandon_restore(&self, live: &Path, cause: SnapshotError) -> SnapshotError {
        let staged = sibling_with_suffix(live, ".restore-staged");
        let cleanup = if std::fs::symlink_metadata(&staged).is_ok() {
            delete_subvolume(&staged)
        } else {
            Ok(())
        };
        match cleanup.and_then(|()| remove_journal(live).map_err(|error| error.to_string())) {
            Ok(()) => cause,
            Err(cleanup) => SnapshotError::RestoreRecovery {
                live: live.to_path_buf(),
                reason: format!("{cause}; cleaning up the staged copy also failed: {cleanup}"),
            },
        }
    }

    /// Settle every interrupted restore under the volumes directory. Bun
    /// runs this at startup, before it adopts or starts any workload.
    /// Returns one error per volume it couldn't settle; those keep their
    /// journal (so the volume refuses to mount) and every copy.
    pub fn recover_restores(&self) -> Vec<SnapshotError> {
        let mut journals = Vec::new();
        // Namespaces are DNS labels; dot directories hold the snapshot
        // store and other bookkeeping, never volumes.
        for namespace in std::fs::read_dir(&self.volumes_dir)
            .into_iter()
            .flatten()
            .flatten()
        {
            let hidden = namespace.file_name().to_string_lossy().starts_with('.');
            if !hidden && namespace.file_type().is_ok_and(|kind| kind.is_dir()) {
                collect_journals(&namespace.path(), &mut journals);
            }
        }
        journals
            .into_iter()
            .filter_map(|journal| {
                let live = journal
                    .to_str()?
                    .strip_suffix(".restore.json")
                    .map(PathBuf::from)?;
                self.recover_volume(&live).err()
            })
            .collect()
    }

    /// Leave exactly one complete volume at `live`, per
    /// [`plan_restore_recovery`], then drop the journal.
    fn recover_volume(&self, live: &Path) -> Result<(), SnapshotError> {
        let staged = sibling_with_suffix(live, ".restore-staged");
        let old = sibling_with_suffix(live, ".restore-old");
        let failed = |reason: String| SnapshotError::RestoreRecovery {
            live: live.to_path_buf(),
            reason,
        };
        let phase = match std::fs::read(restore_journal_path(live)) {
            Ok(bytes) => Some(
                serde_json::from_slice::<RestoreJournal>(&bytes)
                    .map_err(|error| failed(format!("unreadable journal: {error}")))?
                    .phase,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let exists = |path: &Path| std::fs::symlink_metadata(path).is_ok();
        let parent = live.parent().unwrap_or(Path::new("."));
        match plan_restore_recovery(phase, exists(live), exists(&staged), exists(&old)) {
            RestoreRecovery::Clean => return Ok(()),
            RestoreRecovery::DiscardStaged => {
                if exists(&staged) {
                    delete_subvolume(&staged).map_err(failed)?;
                }
            }
            RestoreRecovery::FinishSwap => {
                std::fs::rename(&staged, live)?;
                sync_directory(parent)?;
                delete_subvolume(&old).map_err(failed)?;
            }
            RestoreRecovery::DropOld => {
                if exists(&old) {
                    delete_subvolume(&old).map_err(failed)?;
                }
            }
            RestoreRecovery::ReinstateOld => std::fs::rename(&old, live)?,
            RestoreRecovery::Ambiguous(reason) => return Err(failed(reason.to_string())),
        }
        sync_directory(parent)?;
        remove_journal(live)?;
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

        // An interrupted restore may still need its source snapshot.
        let live = self.volume_host_path(namespace, app, &meta.volume_path);
        if let Ok(bytes) = std::fs::read(restore_journal_path(&live))
            && serde_json::from_slice::<RestoreJournal>(&bytes)
                .map_or(true, |journal| journal.snapshot == meta.name)
        {
            return Err(SnapshotError::RestoreInProgress {
                namespace: namespace.to_string(),
                app: app.to_string(),
                volume: meta.volume_path,
            });
        }

        delete_subvolume(&snapshot_path).map_err(SnapshotError::Btrfs)?;
        match std::fs::remove_file(Self::meta_path(&snapshot_path)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
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

    /// Rewrite a snapshot's metadata (E3 adds export receipts).
    pub fn write_meta(
        &self,
        snapshot_path: &Path,
        meta: &SnapshotMeta,
    ) -> Result<(), SnapshotError> {
        let json = serde_json::to_vec_pretty(meta).map_err(|e| {
            SnapshotError::Btrfs(format!("meta serialise: {e}")) // unreachable in practice
        })?;
        // The export receipts are the upload checkpoint. A torn or lost file
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
    fn volume_slug_is_one_reversible_segment() {
        assert_eq!(volume_slug("/data"), "data");
        assert_eq!(volume_slug("/var/lib/db"), "var%2Flib%2Fdb");
        assert_eq!(volume_slug("/a-b"), "a-b");
        assert_eq!(volume_slug("/100%"), "100%25");
    }

    /// `/a/b` and `/a-b` used to share the slug `a-b`, so one app's two
    /// volumes shared a snapshot directory.
    #[test]
    fn volume_slugs_of_different_paths_never_collide() {
        let paths = [
            "/a/b", "/a-b", "/a--b", "/a/-b", "/a-/b", "/a%2Fb", "/a%b", "/a/%2F", "/ab",
        ];
        let mut slugs: Vec<String> = paths.iter().map(|path| volume_slug(path)).collect();
        slugs.sort();
        slugs.dedup();
        assert_eq!(slugs.len(), paths.len(), "{slugs:?}");
        assert!(slugs.iter().all(|slug| !slug.contains('/')));
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
            schema: SNAPSHOT_META_SCHEMA,
            namespace: "default".to_string(),
            app: "db".to_string(),
            volume_path: "/data".to_string(),
            name: "1752000000".to_string(),
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_752_000_000),
            size_bytes: 4096,
            exports: Vec::new(),
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
            schema: SNAPSHOT_META_SCHEMA,
            namespace: "default".to_string(),
            app: "db".to_string(),
            volume_path: "/data".to_string(),
            name: "1752000000".to_string(),
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_752_000_000),
            size_bytes: 4096,
            exports: Vec::new(),
        };
        manager.write_meta(&snapshot, &meta).unwrap();
        let path = SnapshotManager::meta_path(&snapshot);
        let before = std::fs::metadata(&path).unwrap().ino();
        meta.exports.push(ExportReceipt {
            destination: "file:///backups".to_string(),
            archive: "a.tar.gz".to_string(),
            manifest: "a.json".to_string(),
            sha256: "00".to_string(),
            archive_bytes: 1,
            completed_at: SystemTime::UNIX_EPOCH,
        });
        manager.write_meta(&snapshot, &meta).unwrap();
        // A rename installs a new inode; an in-place write would keep it.
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), before);
        let back: SnapshotMeta = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(back.exported_to("file:///backups"));
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
            schema: SNAPSHOT_META_SCHEMA,
            namespace: "a".to_string(),
            app: "web".to_string(),
            volume_path: volume_path.to_string(),
            name: name.to_string(),
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            size_bytes: 0,
            exports: Vec::new(),
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

    // -- volumes whose old slugs collided (#294) --------------------------

    #[test]
    fn volumes_a_b_and_a_dash_b_keep_separate_snapshots() {
        let (dir, snapshots) = fake_snapshots(&[
            sample_meta("/a/b", "100", 100),
            sample_meta("/a-b", "100", 100),
        ]);
        let nested = snapshots.find("a", "web", "100", Some("/a/b")).unwrap();
        let dashed = snapshots.find("a", "web", "100", Some("/a-b")).unwrap();
        assert_ne!(
            snapshots.snapshot_path(&nested),
            snapshots.snapshot_path(&dashed)
        );
        assert_eq!(snapshots.list("a", "web").unwrap().len(), 2);

        // Deleting one volume's copy leaves the other's alone. (Plain
        // directories aren't subvolumes, so on this host the delete itself
        // fails; it must fail on the right path.)
        let dashed_before = tree(&snapshots.snapshot_path(&dashed));
        match snapshots.delete("a", "web", "100", Some("/a/b")) {
            Ok(()) => {}
            Err(SnapshotError::Btrfs(message)) => assert!(
                message.contains(&*snapshots.snapshot_path(&nested).to_string_lossy()),
                "{message}"
            ),
            Err(other) => panic!("unexpected {other:?}"),
        }
        assert_eq!(tree(&snapshots.snapshot_path(&dashed)), dashed_before);
        assert!(snapshots.find("a", "web", "100", Some("/a-b")).is_ok());
        drop(dir);
    }

    // -- honest inventory (#294) -----------------------------------------

    #[test]
    fn a_truncated_meta_json_is_an_inventory_error() {
        let good = sample_meta("/data", "100", 100);
        let bad = sample_meta("/data", "200", 200);
        let (_dir, snapshots) = fake_snapshots(&[good, bad.clone()]);
        let meta_file = SnapshotManager::meta_path(&snapshots.snapshot_path(&bad));
        let bytes = std::fs::read(&meta_file).unwrap();
        std::fs::write(&meta_file, &bytes[..bytes.len() / 2]).unwrap();

        match snapshots.list("a", "web") {
            Err(SnapshotError::Inventory { path, .. }) => assert_eq!(path, meta_file),
            other => panic!("a torn meta.json must not shorten the list: {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_volume_directory_is_an_inventory_error() {
        let (dir, snapshots) = fake_snapshots(&[sample_meta("/data", "100", 100)]);
        // A file where a volume's snapshot directory belongs can't be read
        // as a directory, even by root.
        let broken = dir.path().join(".snapshots/a/web/wal");
        std::fs::write(&broken, b"not a directory").unwrap();
        assert!(matches!(
            snapshots.list("a", "web"),
            Err(SnapshotError::Inventory { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_volume_directory_without_read_permission_is_an_inventory_error() {
        use std::os::unix::fs::PermissionsExt;
        if nix::unistd::geteuid().is_root() {
            return; // root reads through the mode bits
        }
        let (dir, snapshots) = fake_snapshots(&[sample_meta("/data", "100", 100)]);
        let volume_dir = dir.path().join(".snapshots/a/web/data");
        std::fs::set_permissions(&volume_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = snapshots.list("a", "web");
        std::fs::set_permissions(&volume_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(result, Err(SnapshotError::Inventory { .. })),
            "{result:?}"
        );
    }

    // -- crash-atomic restore (#291, B04) --------------------------------

    #[test]
    fn restore_recovery_leaves_one_complete_volume_for_every_state() {
        use RestorePhase::*;
        use RestoreRecovery::*;
        let cases = [
            // (phase, live, staged, old) -> action
            (None, true, false, false, Clean),
            (None, false, false, false, Clean),
            (None, true, true, false, Ambiguous("")),
            (None, true, false, true, Ambiguous("")),
            (Some(Staging), true, false, false, DiscardStaged),
            (Some(Staging), true, true, false, DiscardStaged),
            (Some(Staging), false, true, false, Ambiguous("")),
            (Some(Staging), true, true, true, Ambiguous("")),
            (Some(Swapping), true, true, false, DiscardStaged),
            (Some(Swapping), false, true, true, FinishSwap),
            (Some(Swapping), true, false, true, DropOld),
            (Some(Swapping), true, false, false, DropOld),
            (Some(Swapping), false, false, true, ReinstateOld),
            (Some(Swapping), false, false, false, Ambiguous("")),
            (Some(Swapping), true, true, true, Ambiguous("")),
            (Some(Swapping), false, true, false, Ambiguous("")),
        ];
        for (phase, live, staged, old, expected) in cases {
            let got = plan_restore_recovery(phase, live, staged, old);
            let same = match (&got, &expected) {
                (Ambiguous(_), Ambiguous(_)) => true,
                _ => got == expected,
            };
            assert!(
                same,
                "{phase:?} live={live} staged={staged} old={old}: {got:?}"
            );
        }
    }

    /// A restore journal, and whichever copies the test asks for, beside
    /// `a/web`'s `/data` volume (plain directories stand in for subvolumes).
    fn interrupted_restore(
        phase: RestorePhase,
        live: Option<&str>,
        old: Option<&str>,
    ) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let live_path = dir.path().join("a/web/data");
        std::fs::create_dir_all(live_path.parent().unwrap()).unwrap();
        for (path, content) in [
            (live_path.clone(), live),
            (sibling_with_suffix(&live_path, ".restore-old"), old),
        ] {
            if let Some(content) = content {
                std::fs::create_dir_all(&path).unwrap();
                std::fs::write(path.join("state"), content).unwrap();
            }
        }
        write_journal(
            &live_path,
            &RestoreJournal {
                schema: 1,
                snapshot: "100".to_string(),
                phase,
            },
        )
        .unwrap();
        (dir, live_path)
    }

    #[test]
    fn recovery_puts_the_original_back_when_only_it_survives() {
        let (dir, live) = interrupted_restore(RestorePhase::Swapping, None, Some("original"));
        let errors = SnapshotManager::new(dir.path()).recover_restores();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(std::fs::read(live.join("state")).unwrap(), b"original");
        assert!(!restore_journal_path(&live).exists());
        assert!(!sibling_with_suffix(&live, ".restore-old").exists());
    }

    #[test]
    fn ambiguous_recovery_keeps_every_copy_and_the_journal() {
        let (dir, live) =
            interrupted_restore(RestorePhase::Staging, Some("live"), Some("displaced"));
        let errors = SnapshotManager::new(dir.path()).recover_restores();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(matches!(errors[0], SnapshotError::RestoreRecovery { .. }));
        assert_eq!(std::fs::read(live.join("state")).unwrap(), b"live");
        let old = sibling_with_suffix(&live, ".restore-old");
        assert_eq!(std::fs::read(old.join("state")).unwrap(), b"displaced");
        assert!(restore_journal_path(&live).exists());
    }

    #[test]
    fn a_volume_with_an_unrecovered_restore_refuses_to_mount() {
        let (dir, _live) = interrupted_restore(RestorePhase::Swapping, None, Some("original"));
        let volumes = crate::grill::volume::VolumeManager::new(dir.path());
        let result = volumes.create_managed_volume("a", "web", Path::new("/data"), None);
        assert!(
            matches!(
                result,
                Err(crate::grill::volume::VolumeError::RestorePending(_))
            ),
            "{result:?}"
        );
        // Nothing was provisioned in the missing volume's place.
        assert!(!dir.path().join("a/web/data").exists());
    }

    #[test]
    fn deleting_the_source_of_an_unrecovered_restore_is_refused() {
        let (dir, snapshots) = fake_snapshots(&[sample_meta("/data", "100", 100)]);
        std::fs::create_dir_all(dir.path().join("a/web/data")).unwrap();
        write_journal(
            &dir.path().join("a/web/data"),
            &RestoreJournal {
                schema: 1,
                snapshot: "100".to_string(),
                phase: RestorePhase::Swapping,
            },
        )
        .unwrap();
        assert!(matches!(
            snapshots.delete("a", "web", "100", Some("/data")),
            Err(SnapshotError::RestoreInProgress { .. })
        ));
        assert!(
            snapshots
                .snapshot_path(&sample_meta("/data", "100", 100))
                .exists()
        );
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
    #[ignore = "requires Linux root, Btrfs tools, and RELIABURGER_BTRFS_TESTS=1; run with make test-linux"]
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
    #[ignore = "requires Linux root, Btrfs tools, and RELIABURGER_BTRFS_TESTS=1; run with make test-linux"]
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

    /// A loop-mounted Btrfs filesystem for one test, unmounted on drop.
    #[cfg(target_os = "linux")]
    struct ScratchBtrfs {
        mount: PathBuf,
        _scratch: tempfile::TempDir,
    }

    #[cfg(target_os = "linux")]
    impl ScratchBtrfs {
        fn new() -> Self {
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
            Self {
                mount,
                _scratch: scratch,
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for ScratchBtrfs {
        fn drop(&mut self) {
            let _ = std::process::Command::new("umount")
                .arg(&self.mount)
                .status();
        }
    }

    /// B04: kill the restore at every step boundary. Recovery must leave a
    /// complete volume (the original or the restored one) with no stray
    /// copies or journal, and a fresh restore must then succeed.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires Linux root, Btrfs tools, and RELIABURGER_BTRFS_TESTS=1; run with make test-linux"]
    fn btrfs_restore_recovers_from_a_crash_at_every_step() {
        let fs = ScratchBtrfs::new();
        let crashes = [
            RestoreCrash::StagingJournalWritten,
            RestoreCrash::StagedCopyBuilt,
            RestoreCrash::SwappingJournalWritten,
            RestoreCrash::LiveMovedAside,
            RestoreCrash::StagedMovedIn,
            RestoreCrash::OldDropped,
        ];
        for (index, crash) in crashes.into_iter().enumerate() {
            let app = format!("db{index}");
            let volumes = crate::grill::volume::VolumeManager::new(&fs.mount);
            let live = volumes
                .create_managed_volume("default", &app, Path::new("/data"), None)
                .unwrap();
            std::fs::write(live.join("state"), b"v1").unwrap();
            let meta = SnapshotManager::new(&fs.mount)
                .create("default", &app, "/data", None, SystemTime::now())
                .unwrap();
            std::fs::write(live.join("state"), b"garbage").unwrap();

            let mut dying = SnapshotManager::new(&fs.mount);
            dying.crash_at = Some(crash);
            assert!(
                dying.restore("default", &app, &meta.name, None).is_err(),
                "{crash:?}"
            );

            let owner = SnapshotManager::new(&fs.mount);
            let errors = owner.recover_restores();
            assert!(errors.is_empty(), "{crash:?}: {errors:?}");
            let state = std::fs::read(live.join("state")).unwrap();
            assert!(
                state == b"v1" || state == b"garbage",
                "{crash:?}: incomplete volume {state:?}"
            );
            for leftover in [
                restore_journal_path(&live),
                sibling_with_suffix(&live, ".restore-staged"),
                sibling_with_suffix(&live, ".restore-old"),
            ] {
                assert!(!leftover.exists(), "{crash:?}: {leftover:?} left behind");
            }

            owner.restore("default", &app, &meta.name, None).unwrap();
            assert_eq!(std::fs::read(live.join("state")).unwrap(), b"v1");
        }
    }

    /// Restore swaps in a new subvolume; it must carry the original's qgroup
    /// limit (the review's open quota question).
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires Linux root, Btrfs tools, and RELIABURGER_BTRFS_TESTS=1; run with make test-linux"]
    fn btrfs_restore_keeps_the_volume_quota() {
        let fs = ScratchBtrfs::new();
        let volumes = crate::grill::volume::VolumeManager::new(&fs.mount);
        let live = volumes
            .create_managed_volume("default", "db", Path::new("/data"), Some("64Mi"))
            .unwrap();
        let snapshots = SnapshotManager::new(&fs.mount);
        let meta = snapshots
            .create("default", "db", "/data", None, SystemTime::now())
            .unwrap();
        snapshots
            .restore("default", "db", &meta.name, None)
            .unwrap();

        let output = std::process::Command::new("btrfs")
            .args(["qgroup", "show", "-rf", "--raw"])
            .arg(&live)
            .output()
            .unwrap();
        let shown = String::from_utf8_lossy(&output.stdout);
        assert!(
            shown.contains(&(64u64 * 1024 * 1024).to_string()),
            "restored volume lost its quota:\n{shown}"
        );
    }

    /// #294 on real Btrfs: `/a/b` and `/a-b` of one app snapshot, list,
    /// restore and delete independently.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires Linux root, Btrfs tools, and RELIABURGER_BTRFS_TESTS=1; run with make test-linux"]
    fn btrfs_volumes_with_formerly_colliding_slugs_stay_separate() {
        let fs = ScratchBtrfs::new();
        let volumes = crate::grill::volume::VolumeManager::new(&fs.mount);
        let nested = volumes
            .create_managed_volume("default", "db", Path::new("/a/b"), None)
            .unwrap();
        let dashed = volumes
            .create_managed_volume("default", "db", Path::new("/a-b"), None)
            .unwrap();
        std::fs::write(nested.join("state"), b"nested").unwrap();
        std::fs::write(dashed.join("state"), b"dashed").unwrap();

        let snapshots = SnapshotManager::new(&fs.mount);
        let metas = snapshots
            .create_for_app("default", "db", None, None, SystemTime::now())
            .unwrap();
        assert_eq!(metas.len(), 2);
        assert_eq!(snapshots.list("default", "db").unwrap().len(), 2);
        let name = metas[0].name.clone();

        std::fs::write(nested.join("state"), b"garbage").unwrap();
        std::fs::write(dashed.join("state"), b"garbage").unwrap();
        snapshots
            .restore("default", "db", &name, Some("/a-b"))
            .unwrap();
        assert_eq!(std::fs::read(dashed.join("state")).unwrap(), b"dashed");
        assert_eq!(std::fs::read(nested.join("state")).unwrap(), b"garbage");
        snapshots
            .restore("default", "db", &name, Some("/a/b"))
            .unwrap();
        assert_eq!(std::fs::read(nested.join("state")).unwrap(), b"nested");

        snapshots
            .delete("default", "db", &name, Some("/a/b"))
            .unwrap();
        let left = snapshots.list("default", "db").unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].volume_path, "/a-b");
    }
}
