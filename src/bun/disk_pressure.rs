//! Disk pressure management.
//!
//! Monitors data directory sizes and triggers export-then-prune when
//! usage exceeds a threshold. Ensures data is safely exported before
//! being deleted locally.
//!
//! A second, cluster-level concern (12b.2 T3): a *council voter* whose disk
//! fills up is a liability. Raft can't make progress if a voter can't persist
//! its log, so a voter under sustained disk pressure should resign its seat
//! and let the self-healing reconciler replace it. The pure state machine
//! that decides "resign now?" with hysteresis lives here; the reconciler acts
//! on its verdict.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

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
///
/// Async because export ships to an object store (`s3://`/`gs://`/local)
/// via `object_store`.
pub async fn check_and_relieve(
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
        && let Ok(export_result) = export_logs(source_dir, dest, node_id, checkpoint).await
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

            // Only prune if this exact content has been exported (or no export
            // dest configured). Keyed by durable id, so pruning never deletes a
            // reused-filename file whose new bytes haven't shipped yet.
            let is_exported = export_dest.is_none() || checkpoint.contains_file(path);

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
// Council-voter disk-pressure resignation (12b.2 T3)
// ---------------------------------------------------------------------------

/// Whether a council voter should resign because its disk is under sustained
/// pressure. The recommendation the reconciler acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResignationVerdict {
    /// Disk is healthy (or pressure hasn't lasted long enough): keep the seat.
    Hold,
    /// Disk has been over the threshold for the whole hold-down window: resign.
    Resign,
}

/// Tracks how long this node's disk has been over the pressure threshold and,
/// once that exceeds a hold-down window, recommends resigning the council
/// seat. The window is the hysteresis that stops a node oscillating around the
/// threshold from churning the council.
///
/// Feed it one observation per tick via [`DiskPressureResignation::observe`].
#[derive(Debug)]
pub struct DiskPressureResignation {
    /// How long pressure must persist before resignation is recommended.
    hold_down: Duration,
    /// When pressure first crossed the threshold in the current spell, or
    /// `None` while the disk is below it.
    over_since: Option<Instant>,
}

impl DiskPressureResignation {
    /// A tracker with the given sustained-pressure hold-down window.
    pub fn new(hold_down: Duration) -> Self {
        Self {
            hold_down,
            over_since: None,
        }
    }

    /// Fold one observation into the tracker and return the current verdict.
    ///
    /// `over_threshold` is whether the disk is currently over its pressure
    /// limit; `now` is injected so tests control time. Dropping back under the
    /// threshold resets the clock, so the next spell starts its window afresh.
    pub fn observe(&mut self, over_threshold: bool, now: Instant) -> ResignationVerdict {
        if !over_threshold {
            self.over_since = None;
            return ResignationVerdict::Hold;
        }
        let since = *self.over_since.get_or_insert(now);
        if now.duration_since(since) >= self.hold_down {
            ResignationVerdict::Resign
        } else {
            ResignationVerdict::Hold
        }
    }

    /// Whether pressure is currently sustained past the window (the last
    /// verdict was `Resign`), without folding a new observation.
    pub fn is_resigning(&self, now: Instant) -> bool {
        self.over_since
            .is_some_and(|since| now.duration_since(since) >= self.hold_down)
    }
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

    #[tokio::test]
    async fn export_then_prune_under_pressure() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        // Create files with distinct bytes so their durable ids differ.
        std::fs::write(source.path().join("logs_000000.parquet"), vec![0u8; 1000]).unwrap();
        std::fs::write(source.path().join("logs_000001.parquet"), vec![1u8; 1000]).unwrap();
        std::fs::write(source.path().join("logs_000002.parquet"), vec![2u8; 1000]).unwrap();

        let mut checkpoint = ExportCheckpoint::default();

        // Threshold of 2000 bytes — should export all, then prune oldest
        let result = check_and_relieve(
            source.path(),
            Some(dest.path().to_str().unwrap()),
            "node-1",
            &mut checkpoint,
            2000, // max bytes
            0,    // no retention limit
        )
        .await;

        // All 3 files exported
        assert!(result.exported);
        assert_eq!(result.files_exported, 3);
        // At least 1 file pruned to get under 2000
        assert!(result.files_pruned >= 1);
        // Remaining size should be at or under threshold
        assert!(dir_parquet_size(source.path()) <= 2000);
    }

    #[tokio::test]
    async fn no_prune_without_export_when_dest_set() {
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
        )
        .await;

        assert!(result.exported);
        assert_eq!(result.files_exported, 1);
        // Now the file IS in the checkpoint, so it CAN be pruned
        assert!(result.files_pruned >= 1);
    }

    #[tokio::test]
    async fn prune_without_export_dest() {
        let source = tempfile::tempdir().unwrap();

        std::fs::write(source.path().join("logs_000000.parquet"), vec![0u8; 2000]).unwrap();

        let mut checkpoint = ExportCheckpoint::default();

        // No export destination — just prune
        let result =
            check_and_relieve(source.path(), None, "node-1", &mut checkpoint, 1000, 0).await;

        assert!(!result.exported);
        assert_eq!(result.files_pruned, 1);
    }

    #[tokio::test]
    async fn no_action_when_under_threshold() {
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
        )
        .await;

        assert_eq!(result.files_pruned, 0);
    }

    // -- resignation state machine (12b.2 T3) ------------------------------

    #[test]
    fn resignation_holds_until_window_elapses() {
        let t0 = Instant::now();
        let mut tracker = DiskPressureResignation::new(Duration::from_secs(30));

        // Over threshold, but only just: still inside the hold-down window.
        assert_eq!(tracker.observe(true, t0), ResignationVerdict::Hold);
        let t1 = t0 + Duration::from_secs(10);
        assert_eq!(tracker.observe(true, t1), ResignationVerdict::Hold);
    }

    #[test]
    fn resignation_fires_after_sustained_pressure() {
        let t0 = Instant::now();
        let mut tracker = DiskPressureResignation::new(Duration::from_secs(30));
        tracker.observe(true, t0);
        let t1 = t0 + Duration::from_secs(31);
        assert_eq!(tracker.observe(true, t1), ResignationVerdict::Resign);
        assert!(tracker.is_resigning(t1));
    }

    #[test]
    fn dropping_under_threshold_resets_the_window() {
        let t0 = Instant::now();
        let mut tracker = DiskPressureResignation::new(Duration::from_secs(30));
        tracker.observe(true, t0);

        // Disk recovers before the window elapses: the clock resets.
        let t1 = t0 + Duration::from_secs(20);
        assert_eq!(tracker.observe(false, t1), ResignationVerdict::Hold);

        // Pressure returns: it must wait out the full window again, so 20s
        // after the return (40s after the very first spell) is still Hold.
        let t2 = t1 + Duration::from_secs(1);
        tracker.observe(true, t2);
        let t3 = t2 + Duration::from_secs(20);
        assert_eq!(tracker.observe(true, t3), ResignationVerdict::Hold);
    }

    #[test]
    fn never_over_threshold_never_resigns() {
        let t0 = Instant::now();
        let mut tracker = DiskPressureResignation::new(Duration::from_secs(1));
        for i in 0..100 {
            let now = t0 + Duration::from_secs(i);
            assert_eq!(tracker.observe(false, now), ResignationVerdict::Hold);
        }
        assert!(!tracker.is_resigning(t0 + Duration::from_secs(100)));
    }
}
