//! Parquet log export to a destination directory.
//!
//! Each node periodically copies its local Parquet log files to a
//! destination path (local filesystem or object store). A checkpoint
//! file tracks which files have already been exported, enabling
//! incremental export across restarts.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::types::KetchupError;

/// Tracks which Parquet files have been exported.
///
/// Persisted as JSON to a checkpoint file so export is incremental
/// across node restarts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExportCheckpoint {
    /// Set of filenames that have already been exported.
    pub exported_files: HashSet<String>,
}

impl ExportCheckpoint {
    /// Load a checkpoint from a JSON file, or return a default if
    /// the file doesn't exist or is corrupt.
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
}

/// Result of an export operation.
#[derive(Debug, Clone)]
pub struct ExportResult {
    /// Number of Parquet files exported.
    pub files_exported: usize,
    /// Total bytes written.
    pub bytes_written: u64,
}

/// Export local Parquet log files to a destination directory.
///
/// Copies any Parquet files from `source_dir` that haven't been
/// exported yet (according to the checkpoint) to a subdirectory
/// at `{destination}/{node_id}/`. Updates the checkpoint after
/// each successful copy.
pub fn export_logs(
    source_dir: &Path,
    destination: &str,
    node_id: &str,
    checkpoint: &mut ExportCheckpoint,
) -> Result<ExportResult, KetchupError> {
    let dest_dir = PathBuf::from(destination).join(node_id);
    std::fs::create_dir_all(&dest_dir)?;

    let mut files_exported = 0;
    let mut bytes_written = 0u64;

    // List local Parquet files
    let entries = match std::fs::read_dir(source_dir) {
        Ok(entries) => entries,
        Err(_) => {
            return Ok(ExportResult {
                files_exported: 0,
                bytes_written: 0,
            });
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "parquet") {
            continue;
        }

        let filename = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };

        // Skip already-exported files
        if checkpoint.exported_files.contains(&filename) {
            continue;
        }

        // Copy to destination
        let dest_path = dest_dir.join(&filename);
        let bytes = std::fs::copy(&path, &dest_path)?;

        checkpoint.exported_files.insert(filename);
        files_exported += 1;
        bytes_written += bytes;
    }

    Ok(ExportResult {
        files_exported,
        bytes_written,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_copies_parquet_files() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        // Create fake Parquet files in source
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
        .unwrap();

        assert_eq!(result.files_exported, 2);
        assert!(dest.path().join("node-1/logs_000000.parquet").exists());
        assert!(dest.path().join("node-1/logs_000001.parquet").exists());
        assert!(!dest.path().join("node-1/not_parquet.txt").exists());
    }

    #[test]
    fn export_skips_already_exported() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        std::fs::write(source.path().join("logs_000000.parquet"), b"data1").unwrap();
        std::fs::write(source.path().join("logs_000001.parquet"), b"data2").unwrap();

        let mut checkpoint = ExportCheckpoint::default();
        checkpoint
            .exported_files
            .insert("logs_000000.parquet".to_string());

        let result = export_logs(
            source.path(),
            dest.path().to_str().unwrap(),
            "node-1",
            &mut checkpoint,
        )
        .unwrap();

        // Only the new file should be exported
        assert_eq!(result.files_exported, 1);
        assert!(dest.path().join("node-1/logs_000001.parquet").exists());
    }

    #[test]
    fn export_empty_source_produces_no_files() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        let mut checkpoint = ExportCheckpoint::default();
        let result = export_logs(
            source.path(),
            dest.path().to_str().unwrap(),
            "node-1",
            &mut checkpoint,
        )
        .unwrap();

        assert_eq!(result.files_exported, 0);
        assert_eq!(result.bytes_written, 0);
    }

    #[test]
    fn checkpoint_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint.json");

        let mut checkpoint = ExportCheckpoint::default();
        checkpoint
            .exported_files
            .insert("logs_000000.parquet".to_string());
        checkpoint
            .exported_files
            .insert("logs_000001.parquet".to_string());
        checkpoint.save(&path).unwrap();

        let loaded = ExportCheckpoint::load(&path);
        assert_eq!(loaded.exported_files.len(), 2);
        assert!(loaded.exported_files.contains("logs_000000.parquet"));
        assert!(loaded.exported_files.contains("logs_000001.parquet"));
    }

    #[test]
    fn checkpoint_load_missing_file_returns_default() {
        let checkpoint = ExportCheckpoint::load(Path::new("/nonexistent/path.json"));
        assert!(checkpoint.exported_files.is_empty());
    }

    #[test]
    fn incremental_export_across_calls() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        // First export: one file
        std::fs::write(source.path().join("logs_000000.parquet"), b"batch1").unwrap();

        let mut checkpoint = ExportCheckpoint::default();
        let r1 = export_logs(
            source.path(),
            dest.path().to_str().unwrap(),
            "node-1",
            &mut checkpoint,
        )
        .unwrap();
        assert_eq!(r1.files_exported, 1);

        // Add another file
        std::fs::write(source.path().join("logs_000001.parquet"), b"batch2").unwrap();

        // Second export: only the new file
        let r2 = export_logs(
            source.path(),
            dest.path().to_str().unwrap(),
            "node-1",
            &mut checkpoint,
        )
        .unwrap();
        assert_eq!(r2.files_exported, 1);

        // Both files at destination
        assert!(dest.path().join("node-1/logs_000000.parquet").exists());
        assert!(dest.path().join("node-1/logs_000001.parquet").exists());
    }
}
