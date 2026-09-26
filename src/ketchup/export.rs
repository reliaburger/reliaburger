//! Parquet log export to an object store.
//!
//! Each node periodically ships its local Parquet log files to a
//! destination: a local filesystem path, or an `s3://`/`gs://` object
//! store. A checkpoint file tracks which files have already been
//! exported so export is incremental across restarts.
//!
//! # Durable object ids
//!
//! The checkpoint keys each exported file by a *durable id* — the
//! filename plus a hash of its contents — not the filename alone. Log
//! files are named `logs_NNNNNN.parquet` from a counter that resumes
//! past the highest file on disk (`LogStore::new`). Retention pruning can
//! delete every file, which resets that counter to zero, so a later flush
//! reuses `logs_000000.parquet` for *different* bytes. A filename-only
//! checkpoint would skip that reused name forever and silently lose the
//! new logs. Hashing the contents makes the reused name a new object.
//!
//! # One Bun-owned checkpoint
//!
//! Every exporter locks the source directory's checkpoint, reloads its latest
//! state, uploads, and atomically persists before acknowledging success. The
//! lock is shared across Bun, the manual API and offline Relish processes.

use std::collections::HashSet;
use std::path::Path;

use object_store::ObjectStoreExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::types::KetchupError;

/// The checkpoint filename both Bun tasks and `relish logs-export` use, so
/// there is exactly one authoritative record of what has been exported.
pub const CHECKPOINT_FILENAME: &str = "_export_checkpoint.json";

/// Tracks which Parquet files have been exported, by durable id.
///
/// Persisted as JSON so export is incremental across node restarts. The
/// stored ids are `{filename}@{sha256}` (see `durable_id`), so a
/// filename reused after retention pruning is treated as a new object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExportCheckpoint {
    /// Durable ids (`{filename}@{hash}`) that have already been exported.
    pub exported_files: HashSet<String>,
    /// Hash of the destination URL and node prefix these acknowledgements cover.
    pub scope: Option<String>,
}

impl ExportCheckpoint {
    /// Load a checkpoint from a JSON file, or return a default if the file
    /// doesn't exist or is corrupt.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Atomically save a checkpoint snapshot, syncing the file and its directory.
    ///
    /// Export callers must use `export_logs`, which owns locking and persistence;
    /// this low-level snapshot operation does not serialise competing writers.
    pub fn save(&self, path: &Path) -> Result<(), KetchupError> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        crate::sesame::identity::atomic_write_mode(path, json.as_bytes(), Some(0o600))?;
        Ok(())
    }

    /// Whether the file at `path` has already been exported, by durable id.
    ///
    /// The disk-pressure pruner uses this so it only deletes a local file once
    /// that exact content has landed in the object store.
    pub fn contains_file(&self, path: &Path, destination: &str, node_id: &str) -> bool {
        if export_scope(destination, node_id).ok().as_ref() != self.scope.as_ref()
            || self.scope.is_none()
        {
            return false;
        }
        match file_durable_id(path) {
            Ok(id) => self.exported_files.contains(&id),
            Err(_) => false,
        }
    }
}

/// Compute the durable id of a file on disk (its name plus a content hash).
pub fn file_durable_id(path: &Path) -> Result<String, KetchupError> {
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| KetchupError::Io(std::io::Error::other("path has no filename")))?;
    let contents = std::fs::read(path)?;
    Ok(durable_id(filename, &contents))
}

/// Result of an export operation.
#[derive(Debug, Clone, Default)]
pub struct ExportResult {
    /// Number of Parquet files exported.
    pub files_exported: usize,
    /// Total bytes written.
    pub bytes_written: u64,
}

/// The durable id of a file: its name plus the full SHA-256 of its contents.
///
/// Two files sharing a name but not contents (a name reused after
/// retention pruning) get different ids, so the checkpoint never skips
/// genuinely new bytes.
fn durable_id(filename: &str, contents: &[u8]) -> String {
    let hash = hex::encode(Sha256::digest(contents));
    format!("{filename}@{hash}")
}

/// Build the object store and key prefix for a destination.
///
/// A bare path or `file://…` maps to the local filesystem; `s3://…` and
/// `gs://…` map to their cloud backends (credentials come from each
/// backend's standard environment variables). The returned prefix is the
/// path *inside* that store where node subdirectories are written.
fn parse_destination(
    destination: &str,
) -> Result<(Box<dyn object_store::ObjectStore>, object_store::path::Path), KetchupError> {
    let url = destination_url(destination)?;
    // The checkpoint licenses pruning the source, so an upload must be durable
    // before it's acknowledged; `object_storage::open` syncs local writes.
    crate::object_storage::open(&url).map_err(|e| {
        KetchupError::Io(std::io::Error::other(format!(
            "unsupported destination: {e}"
        )))
    })
}

fn export_scope(destination: &str, node_id: &str) -> Result<String, KetchupError> {
    let url = destination_url(destination)?;
    let mut digest = Sha256::new();
    digest.update(url.as_str().as_bytes());
    digest.update([0]);
    digest.update(node_id.as_bytes());
    Ok(hex::encode(digest.finalize()))
}

fn destination_url(destination: &str) -> Result<url::Url, KetchupError> {
    // A bare filesystem path has no scheme; normalise it to a file:// URL so
    // `object_storage::open` picks the LocalFileSystem backend. Existing
    // configs and tests pass plain temp-dir paths, so this stays compatible.
    let url = if destination.contains("://") {
        url::Url::parse(destination)
    } else {
        let absolute = std::path::absolute(destination)
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        url::Url::from_file_path(&absolute).map_err(|_| {
            url::ParseError::RelativeUrlWithoutBase // unreachable: absolute() gives an absolute path
        })
    }
    .map_err(|e| KetchupError::Io(std::io::Error::other(format!("invalid destination: {e}"))))?;

    Ok(url)
}

/// Export local Parquet log files to an object store.
///
/// Ships any `.parquet` files in `source_dir` whose durable id isn't yet in
/// the checkpoint to `{destination}/{node_id}/{sha256}-{filename}`, then records
/// each id. `destination` may be a local path, `file://…`, `s3://…` or
/// `gs://…`. The source must exist. While another exporter holds the checkpoint
/// this returns [`KetchupError::ExportBusy`] without touching anything; that
/// exporter is shipping the same files, so callers can skip or retry.
/// The supplied snapshot is replaced only after uploads and checkpoint persistence
/// succeed; callers must not separately save it over the authoritative file.
pub async fn export_logs(
    source_dir: &Path,
    destination: &str,
    node_id: &str,
    checkpoint: &mut ExportCheckpoint,
) -> Result<ExportResult, KetchupError> {
    let directory = source_dir.to_path_buf();
    let (lock, mut current) = tokio::task::spawn_blocking(move || {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(directory.join("_export_checkpoint.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(KetchupError::ExportBusy),
            Err(std::fs::TryLockError::Error(error)) => return Err(KetchupError::Io(error)),
        }
        let path = directory.join(CHECKPOINT_FILENAME);
        let current = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                KetchupError::Io(std::io::Error::other(format!(
                    "invalid export checkpoint {}: {error}",
                    path.display()
                )))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ExportCheckpoint::default()
            }
            Err(error) => {
                return Err(KetchupError::Io(std::io::Error::other(format!(
                    "read export checkpoint {}: {error}",
                    path.display()
                ))));
            }
        };
        Ok::<_, KetchupError>((lock, current))
    })
    .await
    .map_err(|error| KetchupError::Io(std::io::Error::other(error.to_string())))??;

    let result = export_logs_locked(source_dir, destination, node_id, &mut current).await?;
    let path = source_dir.join(CHECKPOINT_FILENAME);
    // Move the lock into the blocking write: cancellation cannot release it while
    // a previous write is still replacing the authoritative checkpoint.
    let committed = tokio::task::spawn_blocking(move || {
        let _lock = lock;
        current.save(&path).map_err(|error| {
            KetchupError::Io(std::io::Error::other(format!(
                "files exported but checkpoint could not be saved: {error}; retry is safe"
            )))
        })?;
        Ok::<_, KetchupError>(current)
    })
    .await
    .map_err(|error| KetchupError::Io(std::io::Error::other(error.to_string())))??;
    *checkpoint = committed;
    Ok(result)
}

async fn export_logs_locked(
    source_dir: &Path,
    destination: &str,
    node_id: &str,
    checkpoint: &mut ExportCheckpoint,
) -> Result<ExportResult, KetchupError> {
    let (store, prefix) = parse_destination(destination)?;
    let node_prefix = prefix.join(node_id);
    let scope = export_scope(destination, node_id)?;
    if checkpoint.scope.as_ref() != Some(&scope) {
        // Unscoped or differently scoped acknowledgements cannot justify skipping
        // an upload or pruning its source. Immutable object names make retries safe.
        checkpoint.exported_files.clear();
        checkpoint.scope = Some(scope);
    }

    let mut entries = tokio::fs::read_dir(source_dir).await.map_err(|error| {
        KetchupError::Io(std::io::Error::new(
            error.kind(),
            format!("list {}: {error}", source_dir.display()),
        ))
    })?;
    let mut candidates: Vec<String> = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(|error| {
        KetchupError::Io(std::io::Error::new(
            error.kind(),
            format!("read directory {}: {error}", source_dir.display()),
        ))
    })? {
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "parquet")
        {
            continue;
        }
        let kind = entry.file_type().await.map_err(|error| {
            KetchupError::Io(std::io::Error::new(
                error.kind(),
                format!("inspect {}: {error}", path.display()),
            ))
        })?;
        if !kind.is_file() {
            return Err(KetchupError::Io(std::io::Error::other(format!(
                "export source {} is not a regular file",
                path.display()
            ))));
        }
        let name = entry.file_name().into_string().map_err(|name| {
            KetchupError::Io(std::io::Error::other(format!(
                "export filename is not UTF-8: {name:?}"
            )))
        })?;
        candidates.push(name);
    }
    candidates.sort();

    let mut result = ExportResult::default();
    let mut live_ids = HashSet::new();
    for filename in candidates {
        let source_path = source_dir.join(&filename);
        let contents = match tokio::fs::read(&source_path).await {
            Ok(bytes) => bytes,
            // Retention may remove an immutable source between enumeration and read.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(KetchupError::Io(std::io::Error::new(
                    error.kind(),
                    format!("read export source {}: {error}", source_path.display()),
                )));
            }
        };
        let id = durable_id(&filename, &contents);
        live_ids.insert(id.clone());
        if checkpoint.exported_files.contains(&id) {
            continue;
        }

        // Include the generation in the object key, not only the checkpoint.
        // Keep the Parquet extension so archive queries discover every generation.
        let key = node_prefix.clone().join(format!(
            "{}-{filename}",
            hex::encode(Sha256::digest(&contents))
        ));
        let len = contents.len() as u64;
        store
            .put(&key, object_store::PutPayload::from(contents))
            .await
            .map_err(|e| KetchupError::Io(std::io::Error::other(format!("put {filename}: {e}"))))?;

        checkpoint.exported_files.insert(id);
        result.files_exported += 1;
        result.bytes_written += len;
    }

    // Only current source generations can need pruning proof. Retired ids
    // can be forgotten: immutable destination keys make a later retry safe.
    checkpoint.exported_files.retain(|id| live_ids.contains(id));
    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Read every object under `dir` (a local export destination) as a map of
    /// relative path → bytes, so tests can assert on what actually landed.
    fn exported_files(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        if let Ok(node_dirs) = std::fs::read_dir(dir) {
            for node in node_dirs.flatten() {
                if let Ok(files) = std::fs::read_dir(node.path()) {
                    for f in files.flatten() {
                        out.push(f.path());
                    }
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn export_writes_parquet_files_to_local_object_store() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        std::fs::write(source.path().join("logs_000000.parquet"), b"data1").unwrap();
        std::fs::write(source.path().join("logs_000001.parquet"), b"data2").unwrap();
        std::fs::write(source.path().join("not_parquet.txt"), b"ignore").unwrap();

        let mut checkpoint = ExportCheckpoint::default();
        let result = export_logs(
            source.path(),
            dest.path().to_str().unwrap(),
            "node-1",
            &mut checkpoint,
        )
        .await
        .unwrap();

        assert_eq!(result.files_exported, 2);
        assert!(
            dest.path()
                .join(format!(
                    "node-1/{}-logs_000000.parquet",
                    hex::encode(Sha256::digest(b"data1"))
                ))
                .exists()
        );
        assert!(
            dest.path()
                .join(format!(
                    "node-1/{}-logs_000001.parquet",
                    hex::encode(Sha256::digest(b"data2"))
                ))
                .exists()
        );
        assert!(!dest.path().join("node-1/not_parquet.txt").exists());
        // The checkpoint advanced by exactly the two exported ids.
        assert_eq!(checkpoint.exported_files.len(), 2);
    }

    #[tokio::test]
    async fn export_via_file_url_scheme() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("logs_000000.parquet"), b"data").unwrap();

        let file_url = format!("file://{}", dest.path().display());
        let mut checkpoint = ExportCheckpoint::default();
        let result = export_logs(source.path(), &file_url, "node-1", &mut checkpoint)
            .await
            .unwrap();

        assert_eq!(result.files_exported, 1);
        assert_eq!(exported_files(dest.path()).len(), 1);
    }

    #[tokio::test]
    async fn export_skips_already_exported_by_durable_id() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        std::fs::write(source.path().join("logs_000000.parquet"), b"data1").unwrap();
        std::fs::write(source.path().join("logs_000001.parquet"), b"data2").unwrap();

        let mut checkpoint = ExportCheckpoint {
            scope: Some(export_scope(dest.path().to_str().unwrap(), "node-1").unwrap()),
            ..ExportCheckpoint::default()
        };
        // Pre-record the durable id of the first file.
        checkpoint
            .exported_files
            .insert(durable_id("logs_000000.parquet", b"data1"));
        checkpoint
            .save(&source.path().join(CHECKPOINT_FILENAME))
            .unwrap();

        let result = export_logs(
            source.path(),
            dest.path().to_str().unwrap(),
            "node-1",
            &mut checkpoint,
        )
        .await
        .unwrap();

        assert_eq!(result.files_exported, 1);
        assert_eq!(exported_files(dest.path()).len(), 1);
    }

    /// The bug OBS7 called out: a filename reused after retention pruning
    /// (same name, different bytes) must not be skipped by the checkpoint.
    #[tokio::test]
    async fn reused_filename_with_new_contents_is_not_skipped() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let mut checkpoint = ExportCheckpoint::default();

        // First export of logs_000000.parquet.
        std::fs::write(source.path().join("logs_000000.parquet"), b"first batch").unwrap();
        let r1 = export_logs(
            source.path(),
            dest.path().to_str().unwrap(),
            "node-1",
            &mut checkpoint,
        )
        .await
        .unwrap();
        assert_eq!(r1.files_exported, 1);
        let first_path = exported_files(dest.path()).pop().unwrap();
        let first_bytes = std::fs::read(&first_path).unwrap();
        assert_eq!(first_bytes, b"first batch");

        // Retention prunes the local file; the flush counter resets, so a new
        // flush reuses the same NAME for entirely different bytes.
        std::fs::remove_file(source.path().join("logs_000000.parquet")).unwrap();
        std::fs::write(source.path().join("logs_000000.parquet"), b"second batch").unwrap();

        let r2 = export_logs(
            source.path(),
            dest.path().to_str().unwrap(),
            "node-1",
            &mut checkpoint,
        )
        .await
        .unwrap();
        assert_eq!(
            r2.files_exported, 1,
            "reused filename with new bytes was skipped"
        );
        assert_eq!(std::fs::read(&first_path).unwrap(), b"first batch");
        let second_path = exported_files(dest.path())
            .into_iter()
            .find(|path| path != &first_path)
            .unwrap();
        assert_eq!(std::fs::read(second_path).unwrap(), b"second batch");
    }

    #[tokio::test]
    async fn competing_export_is_reported_as_busy_not_as_io_failure() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("logs_000000.parquet"), b"data").unwrap();
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(source.path().join("_export_checkpoint.lock"))
            .unwrap();
        holder.try_lock().unwrap();

        let mut checkpoint = ExportCheckpoint::default();
        let result = export_logs(
            source.path(),
            dest.path().to_str().unwrap(),
            "node-1",
            &mut checkpoint,
        )
        .await;

        assert!(
            matches!(result, Err(KetchupError::ExportBusy)),
            "{result:?}"
        );
        assert!(exported_files(dest.path()).is_empty());
    }

    #[tokio::test]
    async fn export_empty_source_produces_no_files() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        let mut checkpoint = ExportCheckpoint::default();
        let result = export_logs(
            source.path(),
            dest.path().to_str().unwrap(),
            "node-1",
            &mut checkpoint,
        )
        .await
        .unwrap();

        assert_eq!(result.files_exported, 0);
        assert_eq!(result.bytes_written, 0);
    }

    #[tokio::test]
    async fn export_missing_source_cannot_acquire_checkpoint_ownership() {
        let dest = tempfile::tempdir().unwrap();
        let mut checkpoint = ExportCheckpoint::default();
        let result = export_logs(
            Path::new("/nonexistent/source"),
            dest.path().to_str().unwrap(),
            "node-1",
            &mut checkpoint,
        )
        .await;
        assert!(result.is_err());
    }

    #[test]
    fn checkpoint_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint.json");

        let mut checkpoint = ExportCheckpoint::default();
        checkpoint
            .exported_files
            .insert(durable_id("logs_000000.parquet", b"a"));
        checkpoint
            .exported_files
            .insert(durable_id("logs_000001.parquet", b"b"));
        checkpoint.save(&path).unwrap();

        let loaded = ExportCheckpoint::load(&path);
        assert_eq!(loaded.exported_files.len(), 2);
    }

    #[test]
    fn checkpoint_replacement_does_not_truncate_the_previous_inode() {
        use std::io::Read;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint.json");
        let mut checkpoint = ExportCheckpoint::default();
        checkpoint.save(&path).unwrap();
        let mut previous = std::fs::File::open(&path).unwrap();
        checkpoint.exported_files.insert("new receipt".to_string());
        checkpoint.save(&path).unwrap();
        let mut bytes = Vec::new();
        previous.read_to_end(&mut bytes).unwrap();
        let old: ExportCheckpoint = serde_json::from_slice(&bytes).unwrap();
        assert!(old.exported_files.is_empty());
        assert_eq!(ExportCheckpoint::load(&path).exported_files.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn failed_checkpoint_rename_cleans_its_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("preserve"), b"existing data").unwrap();
        assert!(ExportCheckpoint::default().save(&path).is_err());
        assert_eq!(
            std::fs::read(path.join("preserve")).unwrap(),
            b"existing data"
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn checkpoint_load_missing_file_returns_default() {
        let checkpoint = ExportCheckpoint::load(Path::new("/nonexistent/path.json"));
        assert!(checkpoint.exported_files.is_empty());
    }

    #[tokio::test]
    async fn incremental_export_across_calls() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        std::fs::write(source.path().join("logs_000000.parquet"), b"batch1").unwrap();

        let mut checkpoint = ExportCheckpoint::default();
        let r1 = export_logs(
            source.path(),
            dest.path().to_str().unwrap(),
            "node-1",
            &mut checkpoint,
        )
        .await
        .unwrap();
        assert_eq!(r1.files_exported, 1);

        std::fs::write(source.path().join("logs_000001.parquet"), b"batch2").unwrap();

        let r2 = export_logs(
            source.path(),
            dest.path().to_str().unwrap(),
            "node-1",
            &mut checkpoint,
        )
        .await
        .unwrap();
        assert_eq!(r2.files_exported, 1);

        assert_eq!(exported_files(dest.path()).len(), 2);
    }

    /// Pruning trusts the checkpoint, so a local destination must hold the
    /// bytes durably before the checkpoint says so. `object_store` only syncs
    /// local writes when asked, and the fsync itself can't be observed short of
    /// a power cut (`tests/power_cut.rs` does that), so check the configuration.
    #[test]
    fn local_destinations_sync_uploads_before_acknowledging() {
        let destination = tempfile::tempdir().unwrap();
        let bare = destination.path().to_str().unwrap().to_string();
        let url = format!("file://{bare}");
        for destination in [bare, url] {
            let (store, _) = parse_destination(&destination).unwrap();
            assert!(
                format!("{store:?}").contains("fsync: true"),
                "{destination} acknowledges unsynced uploads: {store:?}"
            );
        }
    }

    #[test]
    fn durable_id_differs_for_same_name_different_bytes() {
        let a = durable_id("logs_000000.parquet", b"first");
        let b = durable_id("logs_000000.parquet", b"second");
        assert_ne!(a, b);
    }

    // Real S3/GCS export lives in a named, ignored manual suite so it never
    // silently passes without credentials (test-harness honesty rule).
    #[tokio::test]
    #[ignore = "requires AWS credentials and RELIABURGER_TEST_S3_URL"]
    async fn export_to_real_s3_manual() {
        let Ok(dest) = std::env::var("RELIABURGER_TEST_S3_URL") else {
            panic!("set RELIABURGER_TEST_S3_URL=s3://bucket/prefix to run this suite");
        };
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("logs_000000.parquet"), b"real-s3").unwrap();
        let mut checkpoint = ExportCheckpoint::default();
        let result = export_logs(source.path(), &dest, "node-manual", &mut checkpoint)
            .await
            .unwrap();
        assert_eq!(result.files_exported, 1);
    }
}
