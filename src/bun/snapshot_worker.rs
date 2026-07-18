//! Scheduled volume snapshots and object-store upload (Phase 12, E3).
//!
//! A `tokio::time::interval` loop (the house pattern — no cron parser)
//! whose tick body is a plain async function: snapshot every app with
//! provisioned volumes, prune past the retention count, and ship
//! not-yet-uploaded snapshots to the configured object store as
//! `.tar.gz` archives. Per-app failures never abort the sweep, and the
//! `uploaded` flag in each snapshot's metadata is the checkpoint — a
//! failed upload simply retries next tick.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use object_store::ObjectStoreExt;
use tokio_util::sync::CancellationToken;

use crate::config::node::SnapshotsSection;
use crate::grill::snapshot::{SnapshotManager, SnapshotMeta};
use crate::grill::volume::VolumeManager;

/// Destination for snapshot archives, built from `upload_url`.
/// `object_store` gives one interface over `file://`, `s3://`, and
/// `gs://`; credentials come from each backend's standard environment
/// variables.
pub struct SnapshotUploader {
    store: Box<dyn object_store::ObjectStore>,
    prefix: object_store::path::Path,
}

impl SnapshotUploader {
    pub fn from_url(upload_url: &str) -> Result<Self, String> {
        let parsed = url::Url::parse(upload_url)
            .map_err(|e| format!("invalid upload_url {upload_url}: {e}"))?;
        let (store, prefix) = object_store::parse_url(&parsed)
            .map_err(|e| format!("unsupported upload_url {upload_url}: {e}"))?;
        Ok(Self { store, prefix })
    }

    /// Object key for one snapshot archive.
    fn key_for(&self, meta: &SnapshotMeta) -> object_store::path::Path {
        self.prefix
            .clone()
            .join(meta.namespace.as_str())
            .join(meta.app.as_str())
            .join(crate::grill::snapshot::volume_slug(&meta.volume_path))
            .join(format!("{}.tar.gz", meta.name))
    }
}

/// Snapshots to prune: everything beyond the newest `retain` per
/// (namespace, app, volume) group. Pure — the sweep acts on the plan.
pub fn prune_plan(metas: &[SnapshotMeta], retain: usize) -> Vec<SnapshotMeta> {
    use std::collections::HashMap;
    let mut groups: HashMap<(&str, &str, &str), Vec<&SnapshotMeta>> = HashMap::new();
    for meta in metas {
        groups
            .entry((&meta.namespace, &meta.app, &meta.volume_path))
            .or_default()
            .push(meta);
    }

    let mut doomed = Vec::new();
    for (_, mut group) in groups {
        group.sort_by_key(|meta| std::cmp::Reverse(meta.created_at));
        doomed.extend(group.into_iter().skip(retain).cloned());
    }
    doomed
}

/// What one sweep did. Errors are collected, not fatal — one app's
/// broken volume must not stop another app's backup.
#[derive(Debug, Default)]
pub struct TickReport {
    pub created: usize,
    pub pruned: usize,
    pub uploaded: usize,
    pub errors: Vec<String>,
}

/// One sweep: snapshot, prune, upload. `now` is injected so tests are
/// deterministic; the loop passes the wall clock.
pub async fn snapshot_tick(
    volumes_dir: &Path,
    retain: usize,
    uploader: Option<&SnapshotUploader>,
    now: SystemTime,
) -> TickReport {
    let mut report = TickReport::default();
    let volumes = VolumeManager::new(volumes_dir);
    let snapshots = SnapshotManager::new(volumes_dir);

    // Phase 1: snapshot every provisioned volume of every app.
    for (namespace, app) in volumes.provisioned_apps() {
        for volume_path in volumes.provisioned_volumes(&namespace, &app) {
            match snapshots.create(&namespace, &app, &volume_path, None, now) {
                Ok(_) => report.created += 1,
                Err(e) => report
                    .errors
                    .push(format!("snapshot {namespace}/{app}{volume_path}: {e}")),
            }
        }
    }

    // Phase 2: prune and upload — driven by what's on disk under
    // .snapshots, so snapshots of deleted apps still age out and ship.
    for (namespace, app) in snapshots.apps() {
        let metas = match snapshots.list(&namespace, &app) {
            Ok(metas) => metas,
            Err(e) => {
                report.errors.push(format!("list {namespace}/{app}: {e}"));
                continue;
            }
        };

        for doomed in prune_plan(&metas, retain) {
            match snapshots.delete(&namespace, &app, &doomed.name) {
                Ok(()) => report.pruned += 1,
                Err(e) => report
                    .errors
                    .push(format!("prune {namespace}/{app}/{}: {e}", doomed.name)),
            }
        }

        let Some(uploader) = uploader else { continue };
        let survivors = match snapshots.list(&namespace, &app) {
            Ok(metas) => metas,
            Err(_) => continue, // listed above; already reported
        };
        for meta in survivors.into_iter().filter(|m| !m.uploaded) {
            match upload_snapshot(&snapshots, uploader, &meta).await {
                Ok(()) => report.uploaded += 1,
                Err(e) => report
                    .errors
                    .push(format!("upload {namespace}/{app}/{}: {e}", meta.name)),
            }
        }
    }

    report
}

/// Tar + gzip one snapshot directory (blocking task — CPU-bound) and
/// put it in the object store, then flip the `uploaded` checkpoint.
async fn upload_snapshot(
    snapshots: &SnapshotManager,
    uploader: &SnapshotUploader,
    meta: &SnapshotMeta,
) -> Result<(), String> {
    let snapshot_dir: PathBuf = snapshots.snapshot_path(meta);
    let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        builder
            .append_dir_all(".", &snapshot_dir)
            .map_err(|e| format!("tar: {e}"))?;
        let encoder = builder.into_inner().map_err(|e| format!("tar: {e}"))?;
        encoder.finish().map_err(|e| format!("gzip: {e}"))
    })
    .await
    .map_err(|e| format!("archive task: {e}"))??;

    uploader
        .store
        .put(
            &uploader.key_for(meta),
            object_store::PutPayload::from(bytes),
        )
        .await
        .map_err(|e| format!("put: {e}"))?;

    let mut updated = meta.clone();
    updated.uploaded = true;
    snapshots
        .write_meta(&snapshots.snapshot_path(meta), &updated)
        .map_err(|e| format!("checkpoint: {e}"))
}

/// The long-lived loop. Spawned by the binary when
/// `[storage.snapshots] interval_secs > 0`.
pub async fn run_snapshot_loop(
    volumes_dir: PathBuf,
    config: SnapshotsSection,
    shutdown: CancellationToken,
) {
    let uploader = match &config.upload_url {
        Some(url) => match SnapshotUploader::from_url(url) {
            Ok(uploader) => Some(uploader),
            Err(e) => {
                eprintln!("bun: snapshot upload disabled: {e}");
                None
            }
        },
        None => None,
    };

    let mut ticker =
        tokio::time::interval(std::time::Duration::from_secs(config.interval_secs.max(1)));
    ticker.tick().await; // immediate first tick is skipped
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }
        let report = snapshot_tick(
            &volumes_dir,
            config.retain,
            uploader.as_ref(),
            SystemTime::now(),
        )
        .await;
        for error in &report.errors {
            eprintln!("bun: snapshot sweep: {error}");
        }
        if report.created + report.pruned + report.uploaded > 0 {
            println!(
                "bun: snapshot sweep: {} created, {} pruned, {} uploaded",
                report.created, report.pruned, report.uploaded
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn meta(app: &str, volume: &str, name: &str, secs: u64) -> SnapshotMeta {
        SnapshotMeta {
            schema: 1,
            namespace: "default".to_string(),
            app: app.to_string(),
            volume_path: volume.to_string(),
            name: name.to_string(),
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            size_bytes: 0,
            uploaded: false,
        }
    }

    // -- prune_plan ---------------------------------------------------------

    #[test]
    fn prune_keeps_newest_per_volume() {
        let metas = vec![
            meta("db", "/data", "100", 100),
            meta("db", "/data", "300", 300),
            meta("db", "/data", "200", 200),
        ];
        let doomed = prune_plan(&metas, 2);
        assert_eq!(doomed.len(), 1);
        assert_eq!(doomed[0].name, "100", "the oldest goes");
    }

    #[test]
    fn prune_groups_by_app_and_volume() {
        // Two volumes with two snapshots each, retain 2: nothing goes —
        // retention is per volume, not global.
        let metas = vec![
            meta("db", "/data", "1", 1),
            meta("db", "/data", "2", 2),
            meta("db", "/wal", "1", 1),
            meta("db", "/wal", "2", 2),
        ];
        assert!(prune_plan(&metas, 2).is_empty());
    }

    #[test]
    fn prune_nothing_under_retention() {
        let metas = vec![meta("db", "/data", "1", 1)];
        assert!(prune_plan(&metas, 7).is_empty());
    }

    // -- upload pass ---------------------------------------------------------

    /// Build a fake snapshot on disk (a snapshot dir is just a
    /// directory as far as tar is concerned — no btrfs needed).
    fn fake_snapshot(volumes_dir: &Path, meta: &SnapshotMeta) {
        let snapshots = SnapshotManager::new(volumes_dir);
        let dir = snapshots.snapshot_path(meta);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state"), b"snapshot-content").unwrap();
        snapshots.write_meta(&dir, meta).unwrap();
    }

    #[tokio::test]
    async fn upload_ships_archives_then_skips_them() {
        let volumes_dir = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let snapshot = meta("db", "/data", "1000", 1000);
        fake_snapshot(volumes_dir.path(), &snapshot);

        let uploader =
            SnapshotUploader::from_url(&format!("file://{}", dest.path().display())).unwrap();

        // First sweep uploads the archive and flips the checkpoint…
        let report = snapshot_tick(
            volumes_dir.path(),
            7,
            Some(&uploader),
            SystemTime::UNIX_EPOCH,
        )
        .await;
        assert_eq!(report.uploaded, 1, "errors: {:?}", report.errors);
        let archive = dest
            .path()
            .join("default")
            .join("db")
            .join("data")
            .join("1000.tar.gz");
        assert!(archive.is_file(), "archive missing at {archive:?}");

        // …so the second sweep ships nothing.
        let report = snapshot_tick(
            volumes_dir.path(),
            7,
            Some(&uploader),
            SystemTime::UNIX_EPOCH,
        )
        .await;
        assert_eq!(report.uploaded, 0, "checkpoint must prevent re-upload");
    }

    #[tokio::test]
    async fn sweep_prunes_past_retention() {
        let volumes_dir = tempfile::tempdir().unwrap();
        for (name, secs) in [("100", 100u64), ("200", 200), ("300", 300)] {
            fake_snapshot(volumes_dir.path(), &meta("db", "/data", name, secs));
        }

        // No uploader; retain 1 → two pruned. (Deletion shells btrfs
        // for real subvolumes; these fake dirs make it error — which
        // must be REPORTED, not fatal.) So assert via the plan instead:
        let snapshots = SnapshotManager::new(volumes_dir.path());
        let metas = snapshots.list("default", "db").unwrap();
        assert_eq!(prune_plan(&metas, 1).len(), 2);

        let report = snapshot_tick(volumes_dir.path(), 1, None, SystemTime::UNIX_EPOCH).await;
        // Fake snapshots aren't subvolumes, so the deletes error — the
        // sweep survives and reports them.
        assert_eq!(report.errors.len(), 2);
        assert_eq!(report.created, 0);
    }

    /// A sweep over an app whose volumes can't snapshot (Plain backend
    /// on macOS/dev) reports errors without aborting.
    #[tokio::test]
    async fn sweep_survives_unsnapshotable_volumes() {
        let volumes_dir = tempfile::tempdir().unwrap();
        let volumes = VolumeManager::new(volumes_dir.path());
        volumes
            .create_managed_volume("default", "db", Path::new("/data"), None)
            .unwrap();

        let report = snapshot_tick(volumes_dir.path(), 7, None, SystemTime::UNIX_EPOCH).await;
        assert_eq!(report.created, 0);
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("not a btrfs subvolume"));
    }
}
