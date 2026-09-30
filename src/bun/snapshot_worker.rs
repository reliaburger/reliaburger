//! Scheduled volume snapshots and object-store export (Phase 12, E3).
//!
//! A `tokio::time::interval` loop (the house pattern — no cron parser)
//! whose tick body is a plain async function: snapshot every app with
//! provisioned volumes, export whatever the configured destination hasn't
//! confirmed, then prune past the retention count. Per-app failures never
//! abort the sweep.
//!
//! Each export streams the snapshot through tar and gzip into a spool file
//! on the volumes filesystem, hashing as it goes, then uploads the spool in
//! fixed-size parts. Memory stays at one part whatever the volume's size.
//! The archive's object key is its SHA-256 under the node's and volume's own
//! prefix, so two nodes (or two snapshots reusing a name) can't overwrite
//! each other. A per-destination receipt in the snapshot's metadata is the
//! checkpoint: without one for the current destination the snapshot ships
//! again, and pruning never deletes a snapshot that still needs shipping.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::config::node::SnapshotsSection;
use crate::grill::snapshot::{ExportReceipt, SnapshotManager, SnapshotMeta};
use crate::grill::volume::VolumeManager;

/// Size of each uploaded part, and so the most archive data held in memory.
/// S3 needs every part but the last to be at least 5 MiB.
pub const DEFAULT_PART_BYTES: usize = 8 * 1024 * 1024;

/// How long an abandoned multipart upload may take to abort.
const ABORT_TIMEOUT: Duration = Duration::from_secs(30);

/// Spool directory for archives on their way out, beside the volumes they
/// were taken from. A dot directory, so it's never mistaken for a namespace.
const SPOOL_DIR: &str = ".snapshot-spool";

/// Version of the manifest published beside each archive.
const MANIFEST_SCHEMA: u32 = 1;

/// Destination for snapshot archives, built from `upload_url`.
/// `object_store` gives one interface over `file://`, `s3://`, and
/// `gs://`; credentials come from each backend's standard environment
/// variables.
pub struct SnapshotUploader {
    store: Arc<dyn ObjectStore>,
    prefix: object_store::path::Path,
    /// Stable identity of the destination, recorded in receipts.
    destination: String,
    /// This node's name: archives from different nodes never share a key.
    node: String,
    part_bytes: usize,
    /// Deadline for each stage (spooling, uploading) of one export.
    stage_timeout: Duration,
    /// Most bytes one spooled archive may use; `None` keeps 5% of the
    /// spool filesystem free.
    spool_limit: Option<u64>,
}

impl SnapshotUploader {
    /// An uploader for `upload_url`, exporting as `node`.
    pub fn from_url(upload_url: &str, node: &str, stage_timeout: Duration) -> Result<Self, String> {
        let parsed = url::Url::parse(upload_url)
            .map_err(|e| format!("invalid upload_url {upload_url}: {e}"))?;
        let (store, prefix) = crate::object_storage::open(&parsed)
            .map_err(|e| format!("unsupported upload_url {upload_url}: {e}"))?;
        Ok(Self {
            store: Arc::from(store),
            prefix,
            destination: destination_identity(&parsed),
            node: node.to_string(),
            part_bytes: DEFAULT_PART_BYTES,
            stage_timeout,
            spool_limit: None,
        })
    }

    /// The identity receipts record for this destination.
    pub fn destination(&self) -> &str {
        &self.destination
    }

    /// Key prefix for one volume's exports on this node.
    fn volume_prefix(&self, meta: &SnapshotMeta) -> object_store::path::Path {
        self.prefix
            .clone()
            .join(meta.namespace.as_str())
            .join(meta.app.as_str())
            .join(self.node.as_str())
            .join(crate::grill::snapshot::volume_slug(&meta.volume_path))
    }

    /// Content-addressed key of an archive.
    fn archive_key(&self, meta: &SnapshotMeta, sha256: &str) -> object_store::path::Path {
        self.volume_prefix(meta)
            .join("archives")
            .join(format!("sha256-{sha256}.tar.gz"))
    }

    /// Key of the manifest that names an archive: unique per snapshot
    /// (creation time and name) and per content.
    fn manifest_key(&self, meta: &SnapshotMeta, sha256: &str) -> object_store::path::Path {
        let created = meta
            .created_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        self.volume_prefix(meta).join("manifests").join(format!(
            "{created}-{}-{}.json",
            meta.name,
            &sha256[..16]
        ))
    }
}

/// A destination URL without credentials, query or fragment: moving the
/// same bucket to new credentials isn't a new destination, but a different
/// bucket or prefix is.
fn destination_identity(url: &url::Url) -> String {
    let mut identity = url.clone();
    // Both fail only for URLs that can't carry credentials, which is fine.
    let _ = identity.set_username("");
    let _ = identity.set_password(None);
    identity.set_query(None);
    identity.set_fragment(None);
    identity.to_string()
}

/// The pruning decision for one sweep.
#[derive(Debug, Default)]
pub struct PrunePlan {
    /// Past retention and safe to delete.
    pub doomed: Vec<SnapshotMeta>,
    /// Past retention, but the destination hasn't confirmed them yet.
    pub held: Vec<SnapshotMeta>,
}

/// Plan pruning: everything beyond the newest `retain` per (namespace, app,
/// volume) group, except snapshots `destination` hasn't confirmed, which
/// are held until it does. Pure — the sweep acts on the plan.
pub fn prune_plan(metas: &[SnapshotMeta], retain: usize, destination: Option<&str>) -> PrunePlan {
    use std::collections::HashMap;
    let mut groups: HashMap<(&str, &str, &str), Vec<&SnapshotMeta>> = HashMap::new();
    for meta in metas {
        groups
            .entry((&meta.namespace, &meta.app, &meta.volume_path))
            .or_default()
            .push(meta);
    }

    let mut plan = PrunePlan::default();
    for (_, mut group) in groups {
        group.sort_by_key(|meta| std::cmp::Reverse(meta.created_at));
        for meta in group.into_iter().skip(retain) {
            if destination.is_some_and(|destination| !meta.exported_to(destination)) {
                plan.held.push(meta.clone());
            } else {
                plan.doomed.push(meta.clone());
            }
        }
    }
    plan
}

/// What one sweep did. Errors are collected, not fatal — one app's
/// broken volume must not stop another app's backup.
#[derive(Debug, Default)]
pub struct TickReport {
    pub created: usize,
    pub pruned: usize,
    pub uploaded: usize,
    /// Snapshots kept past retention because they haven't shipped yet.
    pub held: usize,
    pub errors: Vec<String>,
}

/// One sweep: snapshot, export, prune. `now` is injected so tests are
/// deterministic; the loop passes the wall clock. `cancel` stops an export
/// in flight without writing its receipt.
pub async fn snapshot_tick(
    volumes_dir: &Path,
    retain: usize,
    uploader: Option<&SnapshotUploader>,
    now: SystemTime,
    cancel: &CancellationToken,
) -> TickReport {
    // The create/list/prune work is synchronous btrfs subprocess + fs walks;
    // run it off the runtime (M7). A panic in the task degrades to a
    // reported error.
    let dir = volumes_dir.to_path_buf();
    let destination = uploader.map(|uploader| uploader.destination().to_string());
    let (mut report, to_upload) = tokio::task::spawn_blocking(move || {
        let mut report = TickReport::default();
        let volumes = VolumeManager::new(&dir);
        let snapshots = SnapshotManager::new(&dir);

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

        // Collect what the destination hasn't confirmed — driven by what's
        // on disk under .snapshots, so snapshots of deleted apps still ship.
        let mut to_upload = Vec::new();
        if let Some(destination) = destination {
            clear_spool(&dir, &mut report);
            for (namespace, app) in snapshots.apps() {
                match snapshots.list(&namespace, &app) {
                    Ok(metas) => to_upload.extend(
                        metas
                            .into_iter()
                            .filter(|meta| !meta.exported_to(&destination)),
                    ),
                    Err(e) => report.errors.push(format!("list {namespace}/{app}: {e}")),
                }
            }
        }
        (report, to_upload)
    })
    .await
    .unwrap_or_else(|_| {
        let mut report = TickReport::default();
        report
            .errors
            .push("snapshot tick task panicked".to_string());
        (report, Vec::new())
    });

    // Phase 2: export before pruning, so a snapshot past retention gets its
    // chance to ship on this very sweep.
    if let Some(uploader) = uploader {
        let snapshots = SnapshotManager::new(volumes_dir);
        for meta in to_upload {
            if cancel.is_cancelled() {
                break;
            }
            match export_snapshot(&snapshots, volumes_dir, uploader, &meta, cancel).await {
                Ok(_) => report.uploaded += 1,
                Err(e) => report.errors.push(format!(
                    "upload {}/{}/{}: {e}",
                    meta.namespace, meta.app, meta.name
                )),
            }
        }
    }

    // Phase 3: prune past retention, holding anything not yet exported.
    let dir = volumes_dir.to_path_buf();
    let destination = uploader.map(|uploader| uploader.destination().to_string());
    match tokio::task::spawn_blocking(move || prune(&dir, retain, destination.as_deref())).await {
        Ok(pruned) => {
            report.pruned += pruned.pruned;
            report.held += pruned.held;
            report.errors.extend(pruned.errors);
        }
        Err(_) => report
            .errors
            .push("snapshot prune task panicked".to_string()),
    }
    report
}

/// Prune every app's snapshots per [`prune_plan`].
fn prune(volumes_dir: &Path, retain: usize, destination: Option<&str>) -> TickReport {
    let mut report = TickReport::default();
    let snapshots = SnapshotManager::new(volumes_dir);
    for (namespace, app) in snapshots.apps() {
        let metas = match snapshots.list(&namespace, &app) {
            Ok(metas) => metas,
            Err(e) => {
                report.errors.push(format!("list {namespace}/{app}: {e}"));
                continue;
            }
        };
        let plan = prune_plan(&metas, retain, destination);
        report.held += plan.held.len();
        for doomed in plan.doomed {
            match snapshots.delete(&namespace, &app, &doomed.name, Some(&doomed.volume_path)) {
                Ok(()) => report.pruned += 1,
                Err(e) => report
                    .errors
                    .push(format!("prune {namespace}/{app}/{}: {e}", doomed.name)),
            }
        }
    }
    report
}

/// Remove spool files a crashed or cancelled export left behind. The sweep
/// is the spool's only user and runs one export at a time.
fn clear_spool(volumes_dir: &Path, report: &mut TickReport) {
    let spool = volumes_dir.join(SPOOL_DIR);
    let Ok(entries) = std::fs::read_dir(&spool) else {
        return;
    };
    for entry in entries.flatten() {
        if let Err(e) = std::fs::remove_file(entry.path()) {
            report
                .errors
                .push(format!("clear spool {}: {e}", entry.path().display()));
        }
    }
}

/// An archive written to the spool, with its identity.
struct SpooledArchive {
    /// Deleted when dropped, whether or not the upload succeeded.
    file: tempfile::NamedTempFile,
    sha256: String,
    bytes: u64,
}

/// How an upload used memory: the largest single part it held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UploadStats {
    parts: usize,
    largest_part: usize,
}

/// The manifest published beside each archive: what it holds and where it
/// came from, so a destination can be browsed without the node.
#[derive(serde::Serialize)]
struct ArchiveManifest<'a> {
    schema: u32,
    node: &'a str,
    namespace: &'a str,
    app: &'a str,
    volume_path: &'a str,
    snapshot: &'a str,
    created_at: SystemTime,
    snapshot_bytes: u64,
    archive: String,
    sha256: &'a str,
    archive_bytes: u64,
}

/// Export one snapshot: spool, upload the archive, publish its manifest,
/// then record the receipt. Nothing is recorded unless all of that
/// succeeded, so a failure, timeout or cancellation simply retries next
/// sweep.
async fn export_snapshot(
    snapshots: &SnapshotManager,
    volumes_dir: &Path,
    uploader: &SnapshotUploader,
    meta: &SnapshotMeta,
    cancel: &CancellationToken,
) -> Result<UploadStats, String> {
    let snapshot_dir = snapshots.snapshot_path(meta);
    let spool_dir = volumes_dir.join(SPOOL_DIR);
    let limit = uploader.spool_limit;
    let deadline = std::time::Instant::now() + uploader.stage_timeout;
    let archive_cancel = cancel.clone();
    let spooled = tokio::task::spawn_blocking(move || {
        spool_archive(&snapshot_dir, &spool_dir, limit, deadline, &archive_cancel)
    })
    .await
    .map_err(|e| format!("archive task: {e}"))??;

    let archive = uploader.archive_key(meta, &spooled.sha256);
    let stats = upload_archive(uploader, &archive, &spooled, cancel).await?;

    let manifest_key = uploader.manifest_key(meta, &spooled.sha256);
    let manifest = ArchiveManifest {
        schema: MANIFEST_SCHEMA,
        node: &uploader.node,
        namespace: &meta.namespace,
        app: &meta.app,
        volume_path: &meta.volume_path,
        snapshot: &meta.name,
        created_at: meta.created_at,
        snapshot_bytes: meta.size_bytes,
        archive: archive.to_string(),
        sha256: &spooled.sha256,
        archive_bytes: spooled.bytes,
    };
    let body = serde_json::to_vec_pretty(&manifest).map_err(|e| format!("manifest: {e}"))?;
    let put = uploader.store.put(&manifest_key, PutPayload::from(body));
    tokio::select! {
        result = tokio::time::timeout(uploader.stage_timeout, put) => {
            result
                .map_err(|_| "manifest put timed out".to_string())?
                .map_err(|e| format!("manifest put: {e}"))?;
        }
        _ = cancel.cancelled() => return Err("cancelled".to_string()),
    }

    let mut updated = meta.clone();
    updated
        .exports
        .retain(|receipt| receipt.destination != uploader.destination);
    updated.exports.push(ExportReceipt {
        destination: uploader.destination.clone(),
        archive: archive.to_string(),
        manifest: manifest_key.to_string(),
        sha256: spooled.sha256.clone(),
        archive_bytes: spooled.bytes,
        completed_at: SystemTime::now(),
    });
    let volumes_dir = volumes_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let snapshots = SnapshotManager::new(&volumes_dir);
        snapshots.write_meta(&snapshots.snapshot_path(&updated), &updated)
    })
    .await
    .map_err(|e| format!("receipt task: {e}"))?
    .map_err(|e| format!("receipt: {e}"))?;
    Ok(stats)
}

/// Tar and gzip a snapshot directory into a fresh spool file, hashing the
/// compressed bytes. Blocking. Fails once `deadline` passes, `cancel`
/// fires, or the archive would outgrow the spool quota; the half-written
/// file is removed either way.
fn spool_archive(
    snapshot_dir: &Path,
    spool_dir: &Path,
    limit: Option<u64>,
    deadline: std::time::Instant,
    cancel: &CancellationToken,
) -> Result<SpooledArchive, String> {
    std::fs::create_dir_all(spool_dir).map_err(|e| format!("spool: {e}"))?;
    let limit = match limit {
        Some(limit) => limit,
        None => spool_quota(spool_dir)?,
    };
    let file = tempfile::Builder::new()
        .suffix(".tar.gz")
        .tempfile_in(spool_dir)
        .map_err(|e| format!("spool: {e}"))?;
    let writer = SpoolWriter {
        inner: std::io::BufWriter::new(file.as_file()),
        hasher: Sha256::new(),
        written: 0,
        limit,
        deadline,
        cancel,
    };
    let encoder = flate2::write::GzEncoder::new(writer, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    builder
        .append_dir_all(".", snapshot_dir)
        .map_err(|e| format!("tar: {e}"))?;
    let encoder = builder.into_inner().map_err(|e| format!("tar: {e}"))?;
    let SpoolWriter {
        mut inner,
        hasher,
        written,
        ..
    } = encoder.finish().map_err(|e| format!("gzip: {e}"))?;
    std::io::Write::flush(&mut inner).map_err(|e| format!("spool: {e}"))?;
    drop(inner);
    Ok(SpooledArchive {
        file,
        sha256: hex::encode(hasher.finalize()),
        bytes: written,
    })
}

/// Bytes a spooled archive may use: whatever is free on the spool's
/// filesystem, less 5% of its size, so an export never fills the disk the
/// volumes live on.
fn spool_quota(spool_dir: &Path) -> Result<u64, String> {
    let stats = nix::sys::statvfs::statvfs(spool_dir).map_err(|e| format!("spool statvfs: {e}"))?;
    let fragment = stats.fragment_size() as u64;
    let available = (stats.blocks_available() as u64).saturating_mul(fragment);
    let total = (stats.blocks() as u64).saturating_mul(fragment);
    Ok(available.saturating_sub(total / 20))
}

/// The spool file's writer: hashes and counts what passes through, and
/// refuses to go on past the quota, the deadline, or a cancellation.
struct SpoolWriter<'a, W: std::io::Write> {
    inner: W,
    hasher: Sha256,
    written: u64,
    limit: u64,
    deadline: std::time::Instant,
    cancel: &'a CancellationToken,
}

impl<W: std::io::Write> std::io::Write for SpoolWriter<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        use std::io::{Error, ErrorKind};
        if self.cancel.is_cancelled() {
            return Err(Error::new(ErrorKind::Interrupted, "export cancelled"));
        }
        if std::time::Instant::now() >= self.deadline {
            return Err(Error::new(ErrorKind::TimedOut, "archiving timed out"));
        }
        if self.written.saturating_add(buffer.len() as u64) > self.limit {
            return Err(Error::new(
                ErrorKind::StorageFull,
                format!("archive exceeds the {} byte spool quota", self.limit),
            ));
        }
        let count = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..count]);
        self.written += count as u64;
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Upload a spooled archive part by part, within the stage deadline.
/// Content addressing makes a repeat harmless: an archive already at its
/// key with the same size is left alone, and one with a different size is
/// a collision that's refused rather than overwritten.
async fn upload_archive(
    uploader: &SnapshotUploader,
    key: &object_store::path::Path,
    spooled: &SpooledArchive,
    cancel: &CancellationToken,
) -> Result<UploadStats, String> {
    let deadline = tokio::time::Instant::now() + uploader.stage_timeout;
    let timed_out = || "upload timed out".to_string();
    match tokio::time::timeout_at(deadline, uploader.store.head(key)).await {
        Err(_) => return Err(timed_out()),
        Ok(Ok(existing)) if existing.size == spooled.bytes => {
            return Ok(UploadStats {
                parts: 0,
                largest_part: 0,
            });
        }
        Ok(Ok(existing)) => {
            return Err(format!(
                "{key} already holds {} bytes, not this archive's {}",
                existing.size, spooled.bytes
            ));
        }
        Ok(Err(object_store::Error::NotFound { .. })) => {}
        Ok(Err(e)) => return Err(format!("head: {e}")),
    }

    let mut upload = tokio::time::timeout_at(deadline, uploader.store.put_multipart(key))
        .await
        .map_err(|_| timed_out())?
        .map_err(|e| format!("put: {e}"))?;
    let mut file = tokio::fs::File::from_std(
        spooled
            .file
            .reopen()
            .map_err(|e| format!("spool reopen: {e}"))?,
    );
    let sent = tokio::select! {
        result = tokio::time::timeout_at(
            deadline,
            send_parts(upload.as_mut(), &mut file, uploader.part_bytes),
        ) => result.unwrap_or_else(|_| Err(timed_out())),
        _ = cancel.cancelled() => Err("cancelled".to_string()),
    };
    let completed = match sent {
        Ok(stats) => tokio::select! {
            result = tokio::time::timeout_at(deadline, upload.complete()) => result
                .map_err(|_| timed_out())
                .and_then(|result| result.map_err(|e| format!("complete: {e}")))
                .map(|_| stats),
            _ = cancel.cancelled() => Err("cancelled".to_string()),
        },
        Err(e) => Err(e),
    };
    if completed.is_err() {
        // Stores like S3 keep abandoned parts until told otherwise.
        let _ = tokio::time::timeout(ABORT_TIMEOUT, upload.abort()).await;
    }
    completed
}

/// Read the spool in `part_bytes` pieces and upload each before reading the
/// next, so at most one part is in memory.
async fn send_parts(
    upload: &mut dyn object_store::MultipartUpload,
    file: &mut tokio::fs::File,
    part_bytes: usize,
) -> Result<UploadStats, String> {
    let mut stats = UploadStats {
        parts: 0,
        largest_part: 0,
    };
    loop {
        let mut part = Vec::with_capacity(part_bytes);
        while part.len() < part_bytes {
            let read = (&mut *file)
                .take((part_bytes - part.len()) as u64)
                .read_to_end(&mut part)
                .await
                .map_err(|e| format!("spool read: {e}"))?;
            if read == 0 {
                break;
            }
        }
        if part.is_empty() {
            return Ok(stats);
        }
        stats.parts += 1;
        stats.largest_part = stats.largest_part.max(part.len());
        let last = part.len() < part_bytes;
        upload
            .put_part(PutPayload::from(part))
            .await
            .map_err(|e| format!("put part: {e}"))?;
        if last {
            return Ok(stats);
        }
    }
}

/// The long-lived loop. Spawned by the binary when
/// `[storage.snapshots] interval_secs > 0`; `node` names this node in
/// every archive key.
pub async fn run_snapshot_loop(
    volumes_dir: PathBuf,
    config: SnapshotsSection,
    node: String,
    shutdown: CancellationToken,
) {
    let uploader = match &config.upload_url {
        Some(url) => match SnapshotUploader::from_url(
            url,
            &node,
            Duration::from_secs(config.upload_timeout_secs.max(1)),
        ) {
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
            &shutdown,
        )
        .await;
        for error in &report.errors {
            eprintln!("bun: snapshot sweep: {error}");
        }
        if report.held > 0 {
            eprintln!(
                "bun: snapshot sweep: keeping {} snapshot(s) past retention until they upload",
                report.held
            );
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
    use crate::grill::snapshot::SNAPSHOT_META_SCHEMA;
    use futures_util::TryStreamExt;
    use std::io::Read;

    fn meta(app: &str, volume: &str, name: &str, secs: u64) -> SnapshotMeta {
        SnapshotMeta {
            schema: SNAPSHOT_META_SCHEMA,
            namespace: "default".to_string(),
            app: app.to_string(),
            volume_path: volume.to_string(),
            name: name.to_string(),
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            size_bytes: 0,
            exports: Vec::new(),
        }
    }

    fn receipt(destination: &str) -> ExportReceipt {
        ExportReceipt {
            destination: destination.to_string(),
            archive: "a".to_string(),
            manifest: "m".to_string(),
            sha256: "00".to_string(),
            archive_bytes: 1,
            completed_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn names(metas: &[SnapshotMeta]) -> Vec<&str> {
        let mut names: Vec<&str> = metas.iter().map(|meta| meta.name.as_str()).collect();
        names.sort();
        names
    }

    // -- prune_plan ---------------------------------------------------------

    #[test]
    fn prune_keeps_newest_per_volume() {
        let metas = vec![
            meta("db", "/data", "100", 100),
            meta("db", "/data", "300", 300),
            meta("db", "/data", "200", 200),
        ];
        let plan = prune_plan(&metas, 2, None);
        assert_eq!(names(&plan.doomed), vec!["100"], "the oldest goes");
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
        assert!(prune_plan(&metas, 2, None).doomed.is_empty());
    }

    #[test]
    fn prune_nothing_under_retention() {
        let metas = vec![meta("db", "/data", "1", 1)];
        assert!(prune_plan(&metas, 7, None).doomed.is_empty());
    }

    /// B06: pruning must never delete a snapshot the destination hasn't
    /// confirmed, however far past retention it is.
    #[test]
    fn prune_holds_snapshots_the_destination_has_not_confirmed() {
        let mut shipped = meta("db", "/data", "100", 100);
        shipped.exports.push(receipt("s3://b/new"));
        let mut elsewhere = meta("db", "/data", "200", 200);
        elsewhere.exports.push(receipt("s3://b/old"));
        let metas = vec![
            shipped,
            elsewhere,
            meta("db", "/data", "300", 300),
            meta("db", "/data", "400", 400),
        ];
        let plan = prune_plan(&metas, 1, Some("s3://b/new"));
        assert_eq!(names(&plan.doomed), vec!["100"]);
        assert_eq!(names(&plan.held), vec!["200", "300"]);
    }

    #[test]
    fn destination_identity_drops_credentials_and_query() {
        let url = url::Url::parse("s3://key:secret@bucket/prefix?region=x#frag").unwrap();
        assert_eq!(destination_identity(&url), "s3://bucket/prefix");
        let url = url::Url::parse("file:///backups/volumes").unwrap();
        assert_eq!(destination_identity(&url), "file:///backups/volumes");
    }

    // -- export pass -------------------------------------------------------

    /// Build a fake snapshot on disk (a snapshot dir is just a
    /// directory as far as tar is concerned — no btrfs needed).
    fn fake_snapshot(volumes_dir: &Path, meta: &SnapshotMeta, content: &[u8]) {
        let snapshots = SnapshotManager::new(volumes_dir);
        let dir = snapshots.snapshot_path(meta);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state"), content).unwrap();
        snapshots.write_meta(&dir, meta).unwrap();
    }

    fn file_uploader(dest: &Path, node: &str) -> SnapshotUploader {
        SnapshotUploader::from_url(
            &format!("file://{}", dest.display()),
            node,
            Duration::from_secs(60),
        )
        .unwrap()
    }

    /// An uploader over any store, as `node`.
    fn store_uploader(
        store: Arc<dyn ObjectStore>,
        destination: &str,
        node: &str,
    ) -> SnapshotUploader {
        SnapshotUploader {
            store,
            prefix: object_store::path::Path::default(),
            destination: destination.to_string(),
            node: node.to_string(),
            part_bytes: DEFAULT_PART_BYTES,
            stage_timeout: Duration::from_secs(60),
            spool_limit: None,
        }
    }

    async fn tick(
        volumes_dir: &Path,
        retain: usize,
        uploader: Option<&SnapshotUploader>,
    ) -> TickReport {
        snapshot_tick(
            volumes_dir,
            retain,
            uploader,
            SystemTime::UNIX_EPOCH,
            &CancellationToken::new(),
        )
        .await
    }

    /// Every object under a destination, key → bytes.
    async fn objects(store: &dyn ObjectStore) -> Vec<(String, Vec<u8>)> {
        let listed: Vec<_> = store.list(None).try_collect().await.unwrap();
        let mut out = Vec::new();
        for object in listed {
            let bytes = store
                .get(&object.location)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            out.push((object.location.to_string(), bytes.to_vec()));
        }
        out.sort();
        out
    }

    /// The `state` file inside a `.tar.gz` archive.
    fn state_in(archive: &[u8]) -> Vec<u8> {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
        for entry in tar.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap().ends_with("state") {
                let mut content = Vec::new();
                entry.read_to_end(&mut content).unwrap();
                return content;
            }
        }
        panic!("archive has no state file");
    }

    fn archives(objects: &[(String, Vec<u8>)]) -> Vec<&(String, Vec<u8>)> {
        objects
            .iter()
            .filter(|(key, _)| key.ends_with(".tar.gz"))
            .collect()
    }

    fn manifests(objects: &[(String, Vec<u8>)]) -> Vec<serde_json::Value> {
        objects
            .iter()
            .filter(|(key, _)| key.ends_with(".json"))
            .map(|(_, bytes)| serde_json::from_slice(bytes).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn upload_ships_archives_then_skips_them() {
        let volumes_dir = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let snapshot = meta("db", "/data", "1000", 1000);
        fake_snapshot(volumes_dir.path(), &snapshot, b"snapshot-content");
        let uploader = file_uploader(dest.path(), "node-a");

        // First sweep uploads the archive and records the receipt…
        let report = tick(volumes_dir.path(), 7, Some(&uploader)).await;
        assert_eq!(report.uploaded, 1, "errors: {:?}", report.errors);
        let listed = SnapshotManager::new(volumes_dir.path())
            .list("default", "db")
            .unwrap();
        let receipt = &listed[0].exports[0];
        assert_eq!(receipt.destination, uploader.destination());
        // A `file://` store is rooted at `/`, so keys are absolute paths.
        let archive = Path::new("/").join(&receipt.archive);
        assert!(archive.is_file(), "archive missing at {archive:?}");
        let under_volume = dest.path().join("default/db/node-a/data/archives");
        assert!(archive.starts_with(&under_volume), "{archive:?}");
        assert!(Path::new("/").join(&receipt.manifest).is_file());
        assert_eq!(
            state_in(&std::fs::read(archive).unwrap()),
            b"snapshot-content"
        );

        // …so the second sweep ships nothing.
        let report = tick(volumes_dir.path(), 7, Some(&uploader)).await;
        assert_eq!(report.uploaded, 0, "the receipt must prevent re-upload");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
    }

    /// B05: two nodes with the same app, volume and snapshot second, one
    /// shared destination. Both archives must survive with their own bytes.
    #[tokio::test]
    async fn two_nodes_same_second_both_archives_survive() {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let snapshot = meta("db", "/data", "1000", 1000);
        let mut nodes = Vec::new();
        for (node, content) in [("node-a", b"written on a"), ("node-b", b"written on b")] {
            let volumes_dir = tempfile::tempdir().unwrap();
            fake_snapshot(volumes_dir.path(), &snapshot, content);
            let uploader = store_uploader(store.clone(), "memory:///shared", node);
            let report = tick(volumes_dir.path(), 7, Some(&uploader)).await;
            assert_eq!(report.uploaded, 1, "{node}: {:?}", report.errors);
            nodes.push(volumes_dir);
        }

        let objects = objects(store.as_ref()).await;
        let mut contents: Vec<Vec<u8>> = archives(&objects)
            .iter()
            .map(|(_, bytes)| state_in(bytes))
            .collect();
        contents.sort();
        assert_eq!(
            contents,
            vec![b"written on a".to_vec(), b"written on b".to_vec()]
        );
        let mut manifest_nodes: Vec<String> = manifests(&objects)
            .iter()
            .map(|manifest| manifest["node"].as_str().unwrap().to_string())
            .collect();
        manifest_nodes.sort();
        assert_eq!(manifest_nodes, vec!["node-a", "node-b"]);
    }

    /// B05: reusing a custom name for new content must not overwrite the
    /// archive of the earlier snapshot of that name.
    #[tokio::test]
    async fn reusing_a_custom_name_keeps_both_archives() {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let uploader = store_uploader(store.clone(), "memory:///shared", "node-a");
        let volumes_dir = tempfile::tempdir().unwrap();
        let snapshots = SnapshotManager::new(volumes_dir.path());

        let first = meta("db", "/data", "pre-upgrade", 1000);
        fake_snapshot(volumes_dir.path(), &first, b"first");
        assert_eq!(
            tick(volumes_dir.path(), 7, Some(&uploader)).await.uploaded,
            1
        );
        // The operator deletes it and takes a new one under the same name.
        std::fs::remove_dir_all(snapshots.snapshot_path(&first)).unwrap();
        let second = meta("db", "/data", "pre-upgrade", 2000);
        fake_snapshot(volumes_dir.path(), &second, b"second");
        let report = tick(volumes_dir.path(), 7, Some(&uploader)).await;
        assert_eq!(report.uploaded, 1, "{:?}", report.errors);

        let objects = objects(store.as_ref()).await;
        let mut contents: Vec<Vec<u8>> = archives(&objects)
            .iter()
            .map(|(_, bytes)| state_in(bytes))
            .collect();
        contents.sort();
        assert_eq!(contents, vec![b"first".to_vec(), b"second".to_vec()]);
        assert_eq!(manifests(&objects).len(), 2);
    }

    /// B06: a receipt names its destination, so a new destination receives
    /// every retained snapshot, even ones the old one already has.
    #[tokio::test]
    async fn a_new_destination_receives_every_retained_snapshot() {
        let volumes_dir = tempfile::tempdir().unwrap();
        for (name, secs) in [("100", 100u64), ("200", 200), ("300", 300)] {
            fake_snapshot(
                volumes_dir.path(),
                &meta("db", "/data", name, secs),
                name.as_bytes(),
            );
        }
        let first = tempfile::tempdir().unwrap();
        let report = tick(
            volumes_dir.path(),
            7,
            Some(&file_uploader(first.path(), "n")),
        )
        .await;
        assert_eq!(report.uploaded, 3, "{:?}", report.errors);

        let second = tempfile::tempdir().unwrap();
        let moved = file_uploader(second.path(), "n");
        let report = tick(volumes_dir.path(), 7, Some(&moved)).await;
        assert_eq!(report.uploaded, 3, "{:?}", report.errors);
        for meta in SnapshotManager::new(volumes_dir.path())
            .list("default", "db")
            .unwrap()
        {
            assert_eq!(meta.exports.len(), 2, "{meta:?}");
            assert!(meta.exported_to(moved.destination()));
        }
    }

    /// B06: an outage longer than the retention window must not prune
    /// snapshots that never left the node, and recovery ships them all.
    #[tokio::test]
    async fn an_outage_longer_than_retention_prunes_nothing_unexported() {
        let volumes_dir = tempfile::tempdir().unwrap();
        let dest_root = tempfile::tempdir().unwrap();
        let dest = dest_root.path().join("backups");
        std::fs::create_dir_all(&dest).unwrap();
        let uploader = file_uploader(&dest, "n");
        // The destination breaks: a file where its directory was.
        std::fs::remove_dir(&dest).unwrap();
        std::fs::write(&dest, b"outage").unwrap();

        let retain = 1;
        for tick_number in 0..(retain as u64 + 3) {
            let name = format!("{}", 100 + tick_number);
            fake_snapshot(
                volumes_dir.path(),
                &meta("db", "/data", &name, 100 + tick_number),
                name.as_bytes(),
            );
            let report = tick(volumes_dir.path(), retain, Some(&uploader)).await;
            assert_eq!(report.uploaded, 0);
            assert!(
                !report.errors.iter().any(|error| error.starts_with("prune")),
                "an unexported snapshot was pruned: {:?}",
                report.errors
            );
            assert_eq!(report.held, tick_number as usize);
        }
        assert_eq!(
            SnapshotManager::new(volumes_dir.path())
                .list("default", "db")
                .unwrap()
                .len(),
            retain + 3
        );

        // The destination recovers: everything ships, then (and only then)
        // the old ones become prunable. Fake snapshots aren't subvolumes, so
        // each planned delete surfaces as a reported error naming it.
        std::fs::remove_file(&dest).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        let report = tick(volumes_dir.path(), retain, Some(&uploader)).await;
        assert_eq!(report.uploaded, retain + 3, "{:?}", report.errors);
        assert_eq!(report.held, 0);
        let prunes = report
            .errors
            .iter()
            .filter(|error| error.starts_with("prune"))
            .count();
        assert_eq!(prunes, 3, "{:?}", report.errors);
    }

    #[tokio::test]
    async fn sweep_prunes_past_retention() {
        let volumes_dir = tempfile::tempdir().unwrap();
        for (name, secs) in [("100", 100u64), ("200", 200), ("300", 300)] {
            fake_snapshot(volumes_dir.path(), &meta("db", "/data", name, secs), b"x");
        }

        // No uploader; retain 1 → two pruned. (Deletion shells btrfs
        // for real subvolumes; these fake dirs make it error — which
        // must be REPORTED, not fatal.) So assert via the plan instead:
        let snapshots = SnapshotManager::new(volumes_dir.path());
        let metas = snapshots.list("default", "db").unwrap();
        assert_eq!(prune_plan(&metas, 1, None).doomed.len(), 2);

        let report = tick(volumes_dir.path(), 1, None).await;
        // Fake snapshots aren't subvolumes, so the deletes error — the
        // sweep survives and reports them.
        assert_eq!(report.errors.len(), 2);
        assert_eq!(report.created, 0);
    }

    /// B02: a multi-volume snapshot gives every volume the same name. The
    /// sweep used to delete by name alone, so pruning `/data`'s old `100`
    /// could resolve to `/wal`'s `100`, its newest retained backup.
    #[tokio::test]
    async fn sweep_prunes_by_volume_and_name() {
        let volumes_dir = tempfile::tempdir().unwrap();
        for (volume, name, secs) in [
            ("/data", "100", 100u64),
            ("/data", "300", 300),
            ("/wal", "100", 100),
        ] {
            fake_snapshot(volumes_dir.path(), &meta("db", volume, name, secs), b"x");
        }

        let report = tick(volumes_dir.path(), 1, None).await;
        // Fake snapshots aren't subvolumes, so the one planned delete fails;
        // its error names the path it aimed at.
        assert_eq!(report.errors.len(), 1, "errors: {:?}", report.errors);
        let target = Path::new("default").join("db").join("data").join("100");
        assert!(
            report.errors[0].contains(&target.to_string_lossy().into_owned()),
            "prune aimed at the wrong snapshot: {}",
            report.errors[0]
        );
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

        let report = tick(volumes_dir.path(), 7, None).await;
        assert_eq!(report.created, 0);
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("not a btrfs subvolume"));
    }

    /// The receipt stops later ticks re-uploading, so the archive must be
    /// durable before the receipt is written.
    #[test]
    fn local_upload_destination_syncs_before_the_checkpoint() {
        let dest = tempfile::tempdir().unwrap();
        let uploader = file_uploader(dest.path(), "n");
        let store = format!("{:?}", uploader.store);
        assert!(store.contains("fsync: true"), "{store}");
    }

    // -- bounded, deadlined upload (B07) -----------------------------------

    /// Bytes no compressor can shrink.
    fn incompressible(len: usize) -> Vec<u8> {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    /// A large, incompressible snapshot uploads in parts no bigger than the
    /// part size: the archive never sits in memory whole.
    #[tokio::test]
    async fn a_large_incompressible_snapshot_uploads_in_bounded_parts() {
        let volumes_dir = tempfile::tempdir().unwrap();
        let snapshot = meta("db", "/data", "1000", 1000);
        let content = incompressible(3 * 1024 * 1024);
        fake_snapshot(volumes_dir.path(), &snapshot, &content);
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let mut uploader = store_uploader(store.clone(), "memory:///shared", "n");
        uploader.part_bytes = 256 * 1024;

        let snapshots = SnapshotManager::new(volumes_dir.path());
        let stats = export_snapshot(
            &snapshots,
            volumes_dir.path(),
            &uploader,
            &snapshot,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(stats.parts >= 12, "{stats:?}");
        assert!(stats.largest_part <= uploader.part_bytes, "{stats:?}");

        let objects = objects(store.as_ref()).await;
        assert_eq!(state_in(&archives(&objects)[0].1), content);
        // The spool is emptied once the export finishes.
        let spooled = std::fs::read_dir(volumes_dir.path().join(SPOOL_DIR))
            .unwrap()
            .count();
        assert_eq!(spooled, 0);
    }

    fn stalled_store() -> Arc<dyn ObjectStore> {
        Arc::new(object_store::throttle::ThrottledStore::new(
            object_store::memory::InMemory::new(),
            object_store::throttle::ThrottleConfig {
                wait_put_per_call: Duration::from_secs(3600),
                ..Default::default()
            },
        ))
    }

    /// A destination that never answers fails the upload within its
    /// deadline, leaves no receipt, and the next sweep retries it.
    #[tokio::test]
    async fn a_stalled_destination_times_out_and_the_next_sweep_retries() {
        let volumes_dir = tempfile::tempdir().unwrap();
        fake_snapshot(volumes_dir.path(), &meta("db", "/data", "1000", 1000), b"x");
        let mut stalled = store_uploader(stalled_store(), "memory:///shared", "n");
        stalled.stage_timeout = Duration::from_millis(200);

        let started = std::time::Instant::now();
        let report = tick(volumes_dir.path(), 7, Some(&stalled)).await;
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(report.uploaded, 0);
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("timed out")),
            "{:?}",
            report.errors
        );
        let snapshots = SnapshotManager::new(volumes_dir.path());
        assert!(
            snapshots.list("default", "db").unwrap()[0]
                .exports
                .is_empty()
        );

        let healthy = store_uploader(
            Arc::new(object_store::memory::InMemory::new()),
            "memory:///shared",
            "n",
        );
        let report = tick(volumes_dir.path(), 7, Some(&healthy)).await;
        assert_eq!(report.uploaded, 1, "{:?}", report.errors);
    }

    /// Cancelling mid-upload (Bun shutting down) stops the export and
    /// leaves no receipt.
    #[tokio::test]
    async fn cancelling_mid_upload_leaves_no_receipt() {
        let volumes_dir = tempfile::tempdir().unwrap();
        fake_snapshot(volumes_dir.path(), &meta("db", "/data", "1000", 1000), b"x");
        let stalled = store_uploader(stalled_store(), "memory:///shared", "n");
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });

        let report = tokio::time::timeout(
            Duration::from_secs(10),
            snapshot_tick(
                volumes_dir.path(),
                7,
                Some(&stalled),
                SystemTime::UNIX_EPOCH,
                &cancel,
            ),
        )
        .await
        .expect("cancellation must stop the upload");
        assert_eq!(report.uploaded, 0);
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("cancelled")),
            "{:?}",
            report.errors
        );
        let snapshots = SnapshotManager::new(volumes_dir.path());
        assert!(
            snapshots.list("default", "db").unwrap()[0]
                .exports
                .is_empty()
        );
    }

    /// A spool that runs out of room fails the export cleanly: an error,
    /// no receipt, no half-written spool file, nothing uploaded.
    #[tokio::test]
    async fn a_full_spool_fails_cleanly() {
        let volumes_dir = tempfile::tempdir().unwrap();
        fake_snapshot(
            volumes_dir.path(),
            &meta("db", "/data", "1000", 1000),
            &incompressible(256 * 1024),
        );
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let mut uploader = store_uploader(store.clone(), "memory:///shared", "n");
        uploader.spool_limit = Some(64 * 1024);

        let report = tick(volumes_dir.path(), 7, Some(&uploader)).await;
        assert_eq!(report.uploaded, 0);
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("spool quota")),
            "{:?}",
            report.errors
        );
        let snapshots = SnapshotManager::new(volumes_dir.path());
        assert!(
            snapshots.list("default", "db").unwrap()[0]
                .exports
                .is_empty()
        );
        assert!(objects(store.as_ref()).await.is_empty());
        let spooled = std::fs::read_dir(volumes_dir.path().join(SPOOL_DIR))
            .unwrap()
            .count();
        assert_eq!(spooled, 0);
    }
}
