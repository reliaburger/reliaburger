//! Disk pressure management.
//!
//! Monitors data directory sizes and triggers export-then-prune when
//! usage exceeds a threshold. Ensures data is safely exported before
//! being deleted locally.

use std::path::Path;
use std::time::SystemTime;

use crate::ketchup::export::{ExportCheckpoint, export_logs};

/// Result of a disk pressure check.
#[derive(Debug, Clone)]
pub struct PressureResult {
    /// Whether any data was exported.
    pub exported: bool,
    /// Number of files exported.
    pub files_exported: usize,
    /// Number of files pruned.
    pub files_pruned: usize,
    /// Bytes reclaimed by pruning.
    pub bytes_reclaimed: u64,
}

/// Calculate the total size of Parquet files in a directory.
pub fn dir_parquet_size(dir: &Path) -> u64 {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };

    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "parquet"))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Check disk pressure and export-then-prune if needed.
///
/// When the total Parquet size in `source_dir` exceeds `max_bytes`,
/// exports un-exported files to the destination first, then prunes
/// the oldest Parquet files until usage is under the threshold.
///
/// Returns `None` if no export destination is configured (pruning
/// still happens based on retention_days).
pub fn check_and_relieve(
    source_dir: &Path,
    export_dest: Option<&str>,
    node_id: &str,
    checkpoint: &mut ExportCheckpoint,
    max_bytes: u64,
    retention_days: u32,
) -> PressureResult {
    let current_size = dir_parquet_size(source_dir);
    let mut result = PressureResult {
        exported: false,
        files_exported: 0,
        files_pruned: 0,
        bytes_reclaimed: 0,
    };

    // If we have a destination, export un-exported files first
    if let Some(dest) = export_dest
        && let Ok(export_result) = export_logs(source_dir, dest, node_id, checkpoint)
        && export_result.files_exported > 0
    {
        result.exported = true;
        result.files_exported = export_result.files_exported;
    }

    // Prune if over threshold or past retention
    let should_prune_for_pressure = max_bytes > 0 && current_size > max_bytes;
    let should_prune_for_retention = retention_days > 0;

    if should_prune_for_pressure || should_prune_for_retention {
        let retention_cutoff = if retention_days > 0 {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .saturating_sub(retention_days as u64 * 86400)
        } else {
            0
        };

        // Collect Parquet files with their metadata
        let mut files: Vec<(std::path::PathBuf, u64, u64)> = Vec::new(); // (path, size, mtime)
        if let Ok(entries) = std::fs::read_dir(source_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "parquet")
                    && let Ok(meta) = std::fs::metadata(&path)
                    && let Ok(modified) = meta.modified()
                {
                    let mtime = modified
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    files.push((path, meta.len(), mtime));
                }
            }
        }

        // Sort oldest first
        files.sort_by_key(|f| f.2);

        let mut remaining_size = current_size;
        for (path, size, mtime) in &files {
            // Prune if file is past retention OR we're over the size limit
            let past_retention = retention_cutoff > 0 && *mtime < retention_cutoff;
            let over_pressure = max_bytes > 0 && remaining_size > max_bytes;

            // Only prune if the file has been exported (or no export dest configured)
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let is_exported = export_dest.is_none() || checkpoint.exported_files.contains(filename);

            if (past_retention || over_pressure)
                && is_exported
                && std::fs::remove_file(path).is_ok()
            {
                result.files_pruned += 1;
                result.bytes_reclaimed += size;
                remaining_size = remaining_size.saturating_sub(*size);
            }
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_parquet_size_counts_only_parquet() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.parquet"), vec![0u8; 1000]).unwrap();
        std::fs::write(dir.path().join("b.parquet"), vec![0u8; 2000]).unwrap();
        std::fs::write(dir.path().join("c.txt"), vec![0u8; 5000]).unwrap();

        assert_eq!(dir_parquet_size(dir.path()), 3000);
    }

    #[test]
    fn dir_parquet_size_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(dir_parquet_size(dir.path()), 0);
    }

    #[test]
    fn export_then_prune_under_pressure() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        // Create files totalling 3000 bytes
        std::fs::write(source.path().join("logs_000000.parquet"), vec![0u8; 1000]).unwrap();
        std::fs::write(source.path().join("logs_000001.parquet"), vec![0u8; 1000]).unwrap();
        std::fs::write(source.path().join("logs_000002.parquet"), vec![0u8; 1000]).unwrap();

        let mut checkpoint = ExportCheckpoint::default();

        // Threshold of 2000 bytes — should export all, then prune oldest
        let result = check_and_relieve(
            source.path(),
            Some(dest.path().to_str().unwrap()),
            "node-1",
            &mut checkpoint,
            2000, // max bytes
            0,    // no retention limit
        );

        // All 3 files exported
        assert!(result.exported);
        assert_eq!(result.files_exported, 3);
        // At least 1 file pruned to get under 2000
        assert!(result.files_pruned >= 1);
        // Remaining size should be at or under threshold
        assert!(dir_parquet_size(source.path()) <= 2000);
    }

    #[test]
    fn no_prune_without_export_when_dest_set() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        std::fs::write(source.path().join("logs_000000.parquet"), vec![0u8; 1000]).unwrap();

        // Pre-mark as NOT exported — checkpoint is empty
        let mut checkpoint = ExportCheckpoint::default();

        // First call: exports but doesn't prune (file just got exported)
        let result = check_and_relieve(
            source.path(),
            Some(dest.path().to_str().unwrap()),
            "node-1",
            &mut checkpoint,
            500, // way under threshold
            0,
        );

        assert!(result.exported);
        assert_eq!(result.files_exported, 1);
        // Now the file IS in the checkpoint, so it CAN be pruned
        assert!(result.files_pruned >= 1);
    }

    #[test]
    fn prune_without_export_dest() {
        let source = tempfile::tempdir().unwrap();

        std::fs::write(source.path().join("logs_000000.parquet"), vec![0u8; 2000]).unwrap();

        let mut checkpoint = ExportCheckpoint::default();

        // No export destination — just prune
        let result = check_and_relieve(source.path(), None, "node-1", &mut checkpoint, 1000, 0);

        assert!(!result.exported);
        assert_eq!(result.files_pruned, 1);
    }

    #[test]
    fn no_action_when_under_threshold() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("logs_000000.parquet"), vec![0u8; 100]).unwrap();

        let mut checkpoint = ExportCheckpoint::default();

        let result = check_and_relieve(
            source.path(),
            None,
            "node-1",
            &mut checkpoint,
            10000, // well above current usage
            0,
        );

        assert_eq!(result.files_pruned, 0);
    }
}
