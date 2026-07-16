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
//! Only Bun's export task and disk-pressure task write this checkpoint.
//! `relish logs-export` used to keep its own competing checkpoint in the
//! same directory, which double-counted or skipped files depending on
//! interleaving. It now shares this one path (X8).

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::types::KetchupError;

/// The checkpoint filename both Bun tasks and `relish logs-export` use, so
/// there is exactly one authoritative record of what has been exported.
pub const CHECKPOINT_FILENAME: &str = "_export_checkpoint.json";

/// Tracks which Parquet files have been exported, by durable id.
///
/// Persisted as JSON so export is incremental across node restarts. The
/// stored ids are `{filename}@{sha256-prefix}` (see [`durable_id`]), so a
/// filename reused after retention pruning is treated as a new object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExportCheckpoint {
    /// Durable ids (`{filename}@{hash}`) that have already been exported.
    #[serde(default)]
    pub exported_files: HashSet<String>,
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

    /// Save the checkpoint to a JSON file.
    pub fn save(&self, path: &Path) -> Result<(), KetchupError> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| KetchupError::Io(std::io::Error::other(e.to_string())))?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Whether the file at `path` has already been exported, by durable id.
    ///
    /// The disk-pressure pruner uses this so it only deletes a local file once
    /// that exact content has landed in the object store.
    pub fn contains_file(&self, path: &Path) -> bool {
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

/// The durable id of a file: its name plus a short hash of its contents.
///
/// Two files sharing a name but not contents (a name reused after
/// retention pruning) get different ids, so the checkpoint never skips
/// genuinely new bytes.
fn durable_id(filename: &str, contents: &[u8]) -> String {
    let hash = hex::encode(Sha256::digest(contents));
    // 16 hex chars (64 bits) is plenty to distinguish reused filenames
    // without bloating the checkpoint.
    format!("{filename}@{}", &hash[..16])
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
    // A bare filesystem path has no scheme; normalise it to a file:// URL so
    // `object_store::parse_url` picks the LocalFileSystem backend. Existing
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

    object_store::parse_url(&url).map_err(|e| {
        KetchupError::Io(std::io::Error::other(format!(
            "unsupported destination: {e}"
        )))
    })
}

/// Export local Parquet log files to an object store.
///
/// Ships any `.parquet` files in `source_dir` whose durable id isn't yet in
/// the checkpoint to `{destination}/{node_id}/{filename}`, then records
/// each id. `destination` may be a local path, `file://…`, `s3://…` or
/// `gs://…`. The checkpoint advances only for files that actually landed.
pub async fn export_logs(
    source_dir: &Path,
    destination: &str,
    node_id: &str,
    checkpoint: &mut ExportCheckpoint,
) -> Result<ExportResult, KetchupError> {
    let (store, prefix) = parse_destination(destination)?;
    let node_prefix = prefix.child(node_id);

    let entries = match std::fs::read_dir(source_dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(ExportResult::default()),
    };

    // Collect candidate filenames first so the async put loop borrows nothing
    // from the (non-Send) ReadDir iterator across an await point.
    let mut candidates: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "parquet") {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            candidates.push(name.to_string());
        }
    }
    candidates.sort();

    let mut result = ExportResult::default();
    for filename in candidates {
        let source_path = source_dir.join(&filename);
        let contents = match std::fs::read(&source_path) {
            Ok(bytes) => bytes,
            Err(_) => continue, // pruned between listing and read; skip
        };
        let id = durable_id(&filename, &contents);
        if checkpoint.exported_files.contains(&id) {
            continue;
        }

        let key = node_prefix.child(filename.as_str());
        let len = contents.len() as u64;
        store
            .put(&key, object_store::PutPayload::from(contents))
            .await
            .map_err(|e| KetchupError::Io(std::io::Error::other(format!("put {filename}: {e}"))))?;

        checkpoint.exported_files.insert(id);
        result.files_exported += 1;
        result.bytes_written += len;
    }

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
        assert!(dest.path().join("node-1/logs_000000.parquet").exists());
        assert!(dest.path().join("node-1/logs_000001.parquet").exists());
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
        assert!(dest.path().join("node-1/logs_000000.parquet").exists());
    }

    #[tokio::test]
    async fn export_skips_already_exported_by_durable_id() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        std::fs::write(source.path().join("logs_000000.parquet"), b"data1").unwrap();
        std::fs::write(source.path().join("logs_000001.parquet"), b"data2").unwrap();

        let mut checkpoint = ExportCheckpoint::default();
        // Pre-record the durable id of the first file.
        checkpoint
            .exported_files
            .insert(durable_id("logs_000000.parquet", b"data1"));

        let result = export_logs(
            source.path(),
            dest.path().to_str().unwrap(),
            "node-1",
            &mut checkpoint,
        )
        .await
        .unwrap();

        assert_eq!(result.files_exported, 1);
        assert!(dest.path().join("node-1/logs_000001.parquet").exists());
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
        let first_bytes = std::fs::read(dest.path().join("node-1/logs_000000.parquet")).unwrap();
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
        let second_bytes = std::fs::read(dest.path().join("node-1/logs_000000.parquet")).unwrap();
        assert_eq!(second_bytes, b"second batch", "destination not overwritten");
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
    async fn export_missing_source_dir_is_noop() {
        let dest = tempfile::tempdir().unwrap();
        let mut checkpoint = ExportCheckpoint::default();
        let result = export_logs(
            Path::new("/nonexistent/source"),
            dest.path().to_str().unwrap(),
            "node-1",
            &mut checkpoint,
        )
        .await
        .unwrap();
        assert_eq!(result.files_exported, 0);
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
