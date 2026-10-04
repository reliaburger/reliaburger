//! Arrow/DataFusion-based time-series store.
//!
//! Metrics are buffered in memory, converted to Arrow RecordBatches,
//! and queryable via DataFusion SQL. Periodically flushed to Parquet
//! files for persistence. The same architecture as InfluxDB IOx.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use datafusion::arrow::array::{Array, Float64Array, StringArray, UInt64Array};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::prelude::*;
use object_store::ObjectStore;
use sha2::{Digest, Sha256};

use super::scan::{ParquetTable, list_local, list_remote, streaming_session};
use super::types::{MayoError, MetricKey, Sample};

/// Where a [`MayoStore`] reads and writes Parquet.
///
/// `Local` is the default single-node case: a filesystem directory. `Remote`
/// (H8) backs the store with an `object_store` bucket named by
/// `[metrics] object_store_url` (`s3://…`, `gs://…`, or `file://…`), so metrics
/// survive node loss and the same DataFusion queries run over the bucket.
#[derive(Clone)]
enum Backend {
    Local,
    Remote {
        store: Arc<dyn ObjectStore>,
        /// Path *inside* the store where `metrics_*.parquet` files live.
        prefix: object_store::path::Path,
    },
}

/// Parse a `[metrics] object_store_url` into an object store and key prefix.
/// A bare path or `file://…` maps to the local filesystem; `s3://…`/`gs://…`
/// map to their cloud backends (credentials from each backend's standard
/// environment variables). Mirrors Ketchup's log export.
fn parse_object_store(
    destination: &str,
) -> Result<(Arc<dyn ObjectStore>, object_store::path::Path), MayoError> {
    let url = if destination.contains("://") {
        url::Url::parse(destination)
            .map_err(|e| MayoError::ObjectStore(format!("invalid object_store_url: {e}")))?
    } else {
        let absolute = std::path::absolute(destination).map_err(MayoError::Io)?;
        url::Url::from_file_path(&absolute)
            .map_err(|_| MayoError::ObjectStore("could not build file:// url".to_string()))?
    };
    let (store, prefix) = crate::object_storage::open(&url)
        .map_err(|e| MayoError::ObjectStore(format!("unsupported object_store_url: {e}")))?;
    Ok((Arc::from(store), prefix))
}

/// Serialise a RecordBatch to in-memory Parquet bytes (for object-store PUT).
#[cfg(test)]
fn batch_to_parquet_bytes(batch: &RecordBatch) -> Result<Vec<u8>, MayoError> {
    let mut buffer = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut buffer, batch.schema(), None)
        .map_err(|e| MayoError::Arrow(e.to_string()))?;
    writer
        .write(batch)
        .map_err(|e| MayoError::Arrow(e.to_string()))?;
    writer
        .close()
        .map_err(|e| MayoError::Arrow(e.to_string()))?;
    Ok(buffer)
}

/// Most rows one per-app query returns from one node.
pub const APP_QUERY_ROW_LIMIT: usize = 10_000;

/// Most rows one unfiltered `/v1/metrics?name=*` query returns from one node.
pub const ALL_QUERY_ROW_LIMIT: usize = 10_000;

const MAX_PENDING_METRIC_ROWS: usize = 1_000_000;

/// Escape a value for safe interpolation into a single-quoted SQL string
/// literal (M1). DataFusion follows standard SQL: a `'` inside a literal is
/// doubled. Without this, a query param like `x' OR '1'='1` breaks out of
/// the literal and can read other namespaces' data.
pub(crate) fn escape_sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}

/// Arrow schema for the metrics table.
pub fn metrics_schema() -> Schema {
    Schema::new(vec![
        Field::new("timestamp", DataType::UInt64, false),
        Field::new("metric_name", DataType::Utf8, false),
        Field::new("labels", DataType::Utf8, false),
        Field::new("value", DataType::Float64, false),
    ])
}

/// Returns the next flush counter for `data_dir`, one past the highest existing
/// `{prefix}_NNNNNN.parquet` file (or 0 if none). Used so a restart resumes
/// numbering instead of overwriting a previous run's files.
pub(crate) fn next_flush_counter(data_dir: &std::path::Path, prefix: &str) -> u64 {
    let mut max_seen: Option<u64> = None;
    if let Ok(entries) = std::fs::read_dir(data_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix(&format!("{prefix}_"))
                && let Some(digits) = rest.strip_suffix(".parquet")
                && let Ok(n) = digits.parse::<u64>()
            {
                max_seen = Some(max_seen.map_or(n, |m| m.max(n)));
            }
        }
    }
    max_seen.map_or(0, |m| m + 1)
}

/// Write a single RecordBatch to a Parquet file at `path`, durably (M6).
///
/// Writes to a `.tmp` sibling, fsyncs it, atomically renames it into place, and
/// fsyncs the directory — so a crash mid-write can't leave a torn file that a
/// later query treats as valid, and a flush that returned Ok is really on disk.
/// Synchronous (Arrow's writer is blocking), so callers run it on
/// `spawn_blocking` to keep the async runtime free (OBS5/M3).
pub(crate) fn write_batch_parquet(
    path: &std::path::Path,
    batch: &RecordBatch,
) -> Result<(), MayoError> {
    let tmp = path.with_extension("parquet.tmp");
    let file = std::fs::File::create(&tmp).map_err(MayoError::Io)?;
    let mut writer = ArrowWriter::try_new(file, batch.schema(), None)
        .map_err(|e| MayoError::Arrow(e.to_string()))?;
    writer
        .write(batch)
        .map_err(|e| MayoError::Arrow(e.to_string()))?;
    // `into_inner` flushes and hands back the File so we can fsync the bytes
    // before the rename publishes them.
    let file = writer
        .into_inner()
        .map_err(|e| MayoError::Arrow(e.to_string()))?;
    file.sync_all().map_err(MayoError::Io)?;
    std::fs::rename(&tmp, path).map_err(MayoError::Io)?;
    if let Some(dir) = path.parent() {
        // A dir fsync makes the rename itself durable. Best-effort: not every
        // filesystem supports it, and the file bytes are already synced.
        if let Ok(dir_file) = std::fs::File::open(dir) {
            let _ = dir_file.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
#[derive(Debug)]
struct MayoCreationConfirmationFault {
    child: PathBuf,
    failing: std::sync::atomic::AtomicBool,
    visits: std::sync::Mutex<Vec<PathBuf>>,
}

#[cfg(test)]
impl MayoCreationConfirmationFault {
    fn observe(&self, child: &Path) -> Result<(), MayoError> {
        if self.child != child {
            return Ok(());
        }
        self.visits.lock().unwrap().push(child.to_path_buf());
        if self.failing.load(std::sync::atomic::Ordering::Acquire) {
            return Err(MayoError::Io(std::io::Error::other(
                "controlled Mayo creation-entry confirmation failure",
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MayoWriteStage {
    BeforePublication,
    AfterPublication,
}

#[cfg(test)]
#[derive(Debug)]
struct MayoWriteGate {
    stage: MayoWriteStage,
    first: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    finished: tokio::sync::Notify,
    released: std::sync::Mutex<bool>,
    release_local: std::sync::Condvar,
    release_remote: tokio::sync::Semaphore,
    fail_parent_sync: std::sync::atomic::AtomicBool,
    parent_sync_visits: std::sync::atomic::AtomicUsize,
    creation_confirmation_fault: std::sync::Mutex<Option<Arc<MayoCreationConfirmationFault>>>,
}
#[cfg(test)]
impl MayoWriteGate {
    fn new(stage: MayoWriteStage) -> Self {
        Self {
            stage,
            first: std::sync::atomic::AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            finished: tokio::sync::Notify::new(),
            released: std::sync::Mutex::new(false),
            release_local: std::sync::Condvar::new(),
            release_remote: tokio::sync::Semaphore::new(0),
            fail_parent_sync: std::sync::atomic::AtomicBool::new(false),
            parent_sync_visits: std::sync::atomic::AtomicUsize::new(0),
            creation_confirmation_fault: std::sync::Mutex::new(None),
        }
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release_local.notify_all();
        self.release_remote.add_permits(1);
    }
    fn wait_local(&self, stage: MayoWriteStage) {
        if self.stage != stage || self.first.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        self.entered.notify_one();
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.release_local.wait(released).unwrap();
        }
    }
    async fn wait_async(&self, stage: MayoWriteStage) -> bool {
        if self.stage != stage || self.first.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return false;
        }
        self.entered.notify_one();
        self.release_remote.acquire().await.unwrap().forget();
        true
    }
}
#[cfg(test)]
struct ReleaseMayoWriteGate(Arc<MayoWriteGate>);
#[cfg(test)]
impl Drop for ReleaseMayoWriteGate {
    fn drop(&mut self) {
        self.0.release();
    }
}
#[cfg(test)]
struct MayoAttemptFinished(Arc<MayoWriteGate>);
#[cfg(test)]
impl Drop for MayoAttemptFinished {
    fn drop(&mut self) {
        self.0.finished.notify_one();
    }
}

// Keep publication identity and encoded bytes stable across cancellation,
// uncertain PUT and proved collision.

#[derive(Clone)]
struct EncodedMayoPublication {
    payload: object_store::PutPayload,
    digest: [u8; 32],
}

/// An immutable batch retained by its store until publication is confirmed.
/// Clones share encoded bytes, write ownership and completion state. Dropping
/// a caller or its clone does not discard the store's pending rows.
#[derive(Clone)]
pub struct PendingFlush {
    batch: RecordBatch,
    target: FlushTarget,
    publication: String,
    encoded: Arc<std::sync::Mutex<Option<EncodedMayoPublication>>>,
    local_parent_entries: Arc<std::sync::Mutex<Option<Vec<PathBuf>>>>,
    writing: Arc<tokio::sync::Mutex<()>>,
    completed: Arc<std::sync::atomic::AtomicBool>,
    foreign_collision: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    test_gate: Option<Arc<MayoWriteGate>>,
    #[cfg(test)]
    creation_confirmation_fault: Option<Arc<MayoCreationConfirmationFault>>,
}

#[derive(Clone)]
enum FlushTarget {
    Local {
        data_dir: PathBuf,
        path: PathBuf,
    },
    Remote {
        store: Arc<dyn ObjectStore>,
        key: object_store::path::Path,
    },
}

impl PendingFlush {
    fn encode(&self) -> Result<EncodedMayoPublication, MayoError> {
        let mut encoded = self.encoded.lock().map_err(|_| {
            MayoError::Io(std::io::Error::other(
                "Mayo publication encoding cache poisoned",
            ))
        })?;
        if let Some(encoded) = &*encoded {
            return Ok(encoded.clone());
        }
        let properties = datafusion::parquet::file::properties::WriterProperties::builder()
            .set_key_value_metadata(Some(vec![datafusion::parquet::file::metadata::KeyValue {
                key: "reliaburger.mayo.publication".into(),
                value: Some(self.publication.clone()),
            }]))
            .build();
        let mut bytes = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut bytes, self.batch.schema(), Some(properties))
            .map_err(|error| MayoError::Arrow(error.to_string()))?;
        writer
            .write(&self.batch)
            .map_err(|error| MayoError::Arrow(error.to_string()))?;
        writer
            .close()
            .map_err(|error| MayoError::Arrow(error.to_string()))?;
        let publication = EncodedMayoPublication {
            digest: Sha256::digest(&bytes).into(),
            payload: bytes.into(),
        };
        *encoded = Some(publication.clone());
        Ok(publication)
    }

    fn targets(&self, source: &super::scan::ParquetSource) -> bool {
        match (&self.target, source) {
            (FlushTarget::Local { path, .. }, super::scan::ParquetSource::Local(candidate)) => {
                path == candidate
            }
            (
                FlushTarget::Remote { key, .. },
                super::scan::ParquetSource::Remote { location, .. },
            ) => key == location,
            _ => false,
        }
    }

    // A matching key or matching sample values do not establish ownership.
    // Digest includes immutable bytes plus the independent publication ID.
    async fn owns_source(&self, source: &super::scan::ParquetSource) -> Result<bool, MayoError> {
        let pending = self.clone();
        let encoded = tokio::task::spawn_blocking(move || pending.encode())
            .await
            .map_err(|error| MayoError::Io(std::io::Error::other(error.to_string())))??;
        match source {
            super::scan::ParquetSource::Local(path) => {
                let path = path.clone();
                tokio::task::spawn_blocking(move || local_mayo_publication_matches(&path, &encoded))
                    .await
                    .map_err(|error| MayoError::Io(std::io::Error::other(error.to_string())))?
            }
            super::scan::ParquetSource::Remote {
                store,
                location,
                size,
            } => {
                if *size != encoded.payload.content_length() as u64 {
                    return Ok(false);
                }
                let response = match store
                    .get_opts(location, object_store::GetOptions::default())
                    .await
                {
                    Ok(response) => response,
                    Err(object_store::Error::NotFound { .. }) => return Ok(false),
                    Err(error) => return Err(MayoError::ObjectStore(error.to_string())),
                };
                remote_mayo_publication_matches(response, &encoded).await
            }
        }
    }
}

fn mayo_digest_matches(size: usize, digest: Sha256, encoded: &EncodedMayoPublication) -> bool {
    size == encoded.payload.content_length()
        && <[u8; 32]>::from(digest.finalize()) == encoded.digest
}

async fn remote_mayo_publication_matches(
    response: object_store::GetResult,
    encoded: &EncodedMayoPublication,
) -> Result<bool, MayoError> {
    use futures_util::StreamExt;
    if response.meta.size != encoded.payload.content_length() as u64 {
        return Ok(false);
    }
    let mut stream = response.into_stream();
    let mut digest = Sha256::new();
    let mut size = 0usize;
    while let Some(bytes) = stream.next().await {
        let bytes = bytes.map_err(|error| MayoError::ObjectStore(error.to_string()))?;
        size = size.saturating_add(bytes.len());
        if size > encoded.payload.content_length() {
            return Ok(false);
        }
        digest.update(bytes);
    }
    Ok(mayo_digest_matches(size, digest, encoded))
}

fn local_mayo_publication_matches(
    path: &Path,
    encoded: &EncodedMayoPublication,
) -> Result<bool, MayoError> {
    use std::io::Read;
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(MayoError::Io(error)),
    };
    if !metadata.is_file() || metadata.len() != encoded.payload.content_length() as u64 {
        return Ok(false);
    }
    let mut file = std::fs::File::open(path).map_err(MayoError::Io)?;
    let mut chunk = [0; 64 * 1024];
    let mut size = 0usize;
    let mut digest = Sha256::new();
    loop {
        let count = file.read(&mut chunk).map_err(MayoError::Io)?;
        if count == 0 {
            break;
        }
        size = size.saturating_add(count);
        if size > encoded.payload.content_length() {
            return Ok(false);
        }
        digest.update(&chunk[..count]);
    }
    Ok(mayo_digest_matches(size, digest, encoded))
}

// The configured data directory's own entry is always reconfirmed, including
// a fresh store reopening a path left visible by an uncertain old publication.
// A live pending handle also retains its originally missing creation chain.
// Unrelated pre-existing ancestor entries are outside this configured scope.
fn local_mayo_parent_entries(
    pending: &PendingFlush,
    data_dir: &Path,
) -> Result<Vec<PathBuf>, MayoError> {
    let mut retained = pending.local_parent_entries.lock().map_err(|_| {
        MayoError::Io(std::io::Error::other(
            "Mayo directory confirmation cache poisoned",
        ))
    })?;
    if let Some(entries) = &*retained {
        return Ok(entries.clone());
    }
    let mut child = data_dir.to_path_buf();
    let mut entries = Vec::new();
    if let Some(parent) = data_dir.parent() {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        entries.push(parent.to_path_buf());
    }
    loop {
        match std::fs::metadata(&child) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => {
                return Err(MayoError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    "Mayo directory creation path contains a non-directory",
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = child.parent().ok_or_else(|| {
                    MayoError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "Mayo missing directory has no parent",
                    ))
                })?;
                // A single relative component has the current directory as its
                // parent; File::open("") cannot confirm that entry.
                let parent = if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                };
                if entries.last().is_none_or(|entry| entry != parent) {
                    entries.push(parent.to_path_buf());
                }
                child = parent.to_path_buf();
            }
            Err(error) => return Err(MayoError::Io(error)),
        }
    }
    *retained = Some(entries.clone());
    Ok(entries)
}

// Confirm each new ancestor before descent. An existing directory reconfirms
// its immediate parent entry because an earlier failed mkdir may have left it
// visible. Recursion stops at the first existing directory; unrelated existing
// filesystem ancestry is not scanned or synchronized.
fn prepare_local_mayo_directory(
    directory: &Path,
    #[cfg(test)] fault: Option<&MayoCreationConfirmationFault>,
) -> Result<(), MayoError> {
    match std::fs::metadata(directory) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(MayoError::Io(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "Mayo directory creation path contains a non-directory",
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = directory
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                prepare_local_mayo_directory(
                    parent,
                    #[cfg(test)]
                    fault,
                )?;
            }
            if let Err(error) = std::fs::create_dir(directory)
                && (error.kind() != std::io::ErrorKind::AlreadyExists
                    || !std::fs::metadata(directory)
                        .map_err(MayoError::Io)?
                        .is_dir())
            {
                return Err(MayoError::Io(error));
            }
        }
        Err(error) => return Err(MayoError::Io(error)),
    }
    if let Some(parent) = directory.parent() {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        #[cfg(test)]
        if let Some(fault) = fault {
            fault.observe(directory)?;
        }
        std::fs::File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(MayoError::Io)?;
    }
    Ok(())
}

fn write_owned_local_mayo(
    pending: &PendingFlush,
    encoded: &EncodedMayoPublication,
) -> Result<(), MayoError> {
    use std::io::Write;
    let FlushTarget::Local { data_dir, path } = &pending.target else {
        return Err(MayoError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Mayo local writer received a remote publication target",
        )));
    };
    // Freeze the owned creation chain BEFORE preparing directories. A failed parent
    // sync must retain this inventory even though those directories now exist.
    let parent_entries = local_mayo_parent_entries(pending, data_dir)?;
    prepare_local_mayo_directory(
        data_dir,
        #[cfg(test)]
        pending.creation_confirmation_fault.as_deref(),
    )?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".mayo-pending-")
        .tempfile_in(data_dir)
        .map_err(MayoError::Io)?;
    for bytes in &encoded.payload {
        temporary.write_all(bytes).map_err(MayoError::Io)?;
    }
    temporary.as_file().sync_all().map_err(MayoError::Io)?;
    match temporary.persist_noclobber(path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            // A directory blocking publication is an ordinary retryable IO
            // refusal. Only a real foreign file is a proved identity collision.
            if !std::fs::metadata(path).map_err(MayoError::Io)?.is_file() {
                return Err(MayoError::Io(error.error));
            }
            if !local_mayo_publication_matches(path, encoded)? {
                pending
                    .foreign_collision
                    .store(true, std::sync::atomic::Ordering::Release);
                return Err(MayoError::Io(std::io::Error::other(
                    "foreign Mayo publication collision",
                )));
            }
            std::fs::File::open(path)
                .and_then(|file| file.sync_all())
                .map_err(MayoError::Io)?;
        }
        Err(error) => return Err(MayoError::Io(error.error)),
    }
    // Propagate directory sync errors: completed means durable, not merely visible.
    std::fs::File::open(data_dir)
        .and_then(|directory| directory.sync_all())
        .map_err(MayoError::Io)?;
    for parent in parent_entries {
        #[cfg(test)]
        if let Some(fault) = &pending.creation_confirmation_fault {
            let fault_parent = fault.child.parent().map(|parent| {
                if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                }
            });
            if fault_parent == Some(parent.as_path()) {
                fault.observe(&fault.child)?;
            }
        }
        #[cfg(test)]
        if let Some(gate) = &pending.test_gate {
            gate.parent_sync_visits
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            if gate
                .fail_parent_sync
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(MayoError::Io(std::io::Error::other(
                    "controlled Mayo parent-directory confirmation failure",
                )));
            }
        }
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(MayoError::Io)?;
    }
    Ok(())
}

/// Write at most two batches without holding the store lock during I/O.
/// Finish an older pending batch first, then one current buffer snapshot;
/// concurrent collection cannot extend the pass indefinitely. Each pending
/// batch shares a write guard, including blocking work surviving cancellation.
pub async fn flush_off_lock(
    store: &Arc<tokio::sync::RwLock<MayoStore>>,
) -> Result<bool, MayoError> {
    let mut wrote = false;
    for _ in 0..2 {
        let pending = store.write().await.take_flush_batch()?;
        let Some(pending) = pending else { break };
        write_pending_flush(pending).await?;
        store.write().await.clear_completed_flush();
        wrote = true;
    }
    Ok(wrote)
}

/// Publish or verify the retained immutable batch without a store lock.
/// Clones serialize writes through their shared guard. Completion requires
/// successful publication or verified matching bytes and, for local storage,
/// the file and directory-entry confirmations. Cancellation does not clear
/// store ownership; an already running local blocking writer may still finish.
pub async fn write_pending_flush(pending: PendingFlush) -> Result<(), MayoError> {
    use std::sync::atomic::Ordering;
    let writing = pending.writing.clone().lock_owned().await;
    if pending.completed.load(Ordering::Acquire) {
        return Ok(());
    }
    if pending.foreign_collision.load(Ordering::Acquire) {
        return Err(MayoError::ObjectStore(
            "foreign Mayo publication collision; retarget through the store".into(),
        ));
    }
    match &pending.target {
        FlushTarget::Local { .. } => tokio::task::spawn_blocking(move || {
            let _writing = writing;
            #[cfg(test)]
            let _finished = pending
                .test_gate
                .as_ref()
                .map(|gate| MayoAttemptFinished(gate.clone()));
            #[cfg(test)]
            if let Some(gate) = &pending.test_gate {
                gate.wait_local(MayoWriteStage::BeforePublication);
            }
            let encoded = pending.encode()?;
            write_owned_local_mayo(&pending, &encoded)?;
            #[cfg(test)]
            if let Some(gate) = &pending.test_gate {
                gate.wait_local(MayoWriteStage::AfterPublication);
            }
            pending.completed.store(true, Ordering::Release);
            Ok(())
        })
        .await
        .map_err(|error| MayoError::Io(std::io::Error::other(error.to_string())))?,
        FlushTarget::Remote { store, key } => {
            let store = store.clone();
            let key = key.clone();
            let encoder = pending.clone();
            // Move the mutex guard onto the blocking pool during encoding, so
            // cancellation cannot let another attempt overlap that owned work.
            let (writing, encoded) = tokio::task::spawn_blocking(move || {
                let encoded = encoder.encode()?;
                Ok::<_, MayoError>((writing, encoded))
            })
            .await
            .map_err(|error| MayoError::Io(std::io::Error::other(error.to_string())))??;
            let _writing = writing;
            let result = store
                .put_opts(
                    &key,
                    encoded.payload.clone(),
                    object_store::PutOptions {
                        mode: object_store::PutMode::Create,
                        ..Default::default()
                    },
                )
                .await;
            match result {
                Ok(_) => {}
                Err(
                    object_store::Error::AlreadyExists { .. }
                    | object_store::Error::Precondition { .. },
                ) => {
                    let response = store
                        .get_opts(&key, object_store::GetOptions::default())
                        .await
                        .map_err(|error| MayoError::ObjectStore(error.to_string()))?;
                    if !remote_mayo_publication_matches(response, &encoded).await? {
                        pending.foreign_collision.store(true, Ordering::Release);
                        return Err(MayoError::ObjectStore(
                            "foreign Mayo publication collision".into(),
                        ));
                    }
                }
                Err(error) => return Err(MayoError::ObjectStore(error.to_string())),
            }
            pending.completed.store(true, Ordering::Release);
            Ok(())
        }
    }
}
/// Whether `data_dir` contains at least one `.parquet` file.
pub(crate) fn dir_has_parquet(data_dir: &std::path::Path) -> bool {
    std::fs::read_dir(data_dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "parquet"))
        })
        .unwrap_or(false)
}

/// A buffered sample waiting to be flushed.
struct BufferedSample {
    timestamp: u64,
    metric_name: String,
    labels_json: String,
    value: f64,
}

/// Arrow/DataFusion time-series store.
///
/// Inserts go into an in-memory buffer. On flush, the buffer is written to a
/// Parquet file and dropped from memory. Queries stream the Parquet files
/// (durable across restarts) one at a time, unioned with the unflushed
/// buffer, skipping whatever the files' statistics rule out, so a query's
/// memory is its working set regardless of how much history is on disk.
pub struct MayoStore {
    pending: Option<PendingFlush>,
    /// In-memory buffer of unflushed samples.
    buffer: Vec<BufferedSample>,
    /// Local directory for Parquet files (also the checkpoint dir under a
    /// remote backend, which otherwise ignores it).
    data_dir: PathBuf,
    /// Storage backend: a local directory or an object store (H8).
    backend: Backend,
    /// Counter for unique Parquet file names. Seeded past any existing files
    /// so a restart never clobbers a previous run's data.
    flush_counter: u64,
    #[cfg(test)]
    test_gate: Option<Arc<MayoWriteGate>>,
}

impl MayoStore {
    /// Open (or create) a store writing Parquet to a local `data_dir`.
    ///
    /// Existing `metrics_NNNNNN.parquet` files are left in place and remain
    /// queryable; the flush counter resumes past the highest one so restarts
    /// don't overwrite them.
    pub fn new(data_dir: PathBuf) -> Self {
        let flush_counter = next_flush_counter(&data_dir, "metrics");
        Self {
            pending: None,
            buffer: Vec::new(),
            data_dir,
            backend: Backend::Local,
            flush_counter,
            #[cfg(test)]
            test_gate: None,
        }
    }

    /// Open a store, backing it with an object store when `object_store_url` is
    /// set (H8) or a local `data_dir` otherwise. Generic opens read the whole
    /// configured archive. Remote chunks use fresh random 128-bit names and
    /// create-only PUTs, so simultaneous writers never replace existing data.
    pub async fn open(
        data_dir: PathBuf,
        object_store_url: Option<&str>,
    ) -> Result<Self, MayoError> {
        let Some(url) = object_store_url.filter(|u| !u.is_empty()) else {
            return Ok(Self::new(data_dir));
        };
        let (store, prefix) = parse_object_store(url)?;
        let flush_counter = 0;
        Ok(Self {
            pending: None,
            buffer: Vec::new(),
            data_dir,
            backend: Backend::Remote { store, prefix },
            flush_counter,
            #[cfg(test)]
            test_gate: None,
        })
    }

    /// Open a production node's archive within the configured bucket. The
    /// stable opaque owner is hashed, so labels and path characters cannot
    /// accidentally merge writers. Generic `open` still reads the full archive.
    pub async fn open_for_node(
        data_dir: PathBuf,
        object_store_url: Option<&str>,
        owner: &str,
    ) -> Result<Self, MayoError> {
        let mut store = Self::open(data_dir, object_store_url).await?;
        if let Backend::Remote { prefix, .. } = &mut store.backend {
            if owner.is_empty() {
                return Err(MayoError::ObjectStore(
                    "node archive owner is required".into(),
                ));
            }
            *prefix = prefix
                .clone()
                .join(format!("nodes/{:x}", Sha256::digest(owner.as_bytes())).as_str());
        }
        Ok(store)
    }

    /// The local directory where Parquet files are stored (the configured
    /// metrics dir; unused for storage under a remote backend).
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Insert a metric sample into the buffer.
    pub fn insert(&mut self, key: &MetricKey, sample: Sample) {
        self.clear_completed_flush();
        self.buffer.push(BufferedSample {
            timestamp: sample.timestamp,
            metric_name: key.name.0.clone(),
            labels_json: key.labels_json(),
            value: sample.value,
        });
        let reserved = self
            .pending
            .as_ref()
            .map_or(0, |pending| pending.batch.num_rows());
        let available = MAX_PENDING_METRIC_ROWS.saturating_sub(reserved);
        if self.buffer.len() > available {
            let overflow = self.buffer.len() - available;
            self.buffer.drain(0..overflow);
        }
    }

    /// Insert with the current timestamp (convenience).
    pub fn insert_now(&mut self, key: &MetricKey, value: f64) {
        self.insert(key, Sample::now(value));
    }

    /// Number of unflushed samples in the buffer.
    pub fn buffer_len(&self) -> usize {
        self.buffer.len()
            + self
                .pending
                .as_ref()
                .filter(|pending| !pending.completed.load(std::sync::atomic::Ordering::Acquire))
                .map_or(0, |pending| pending.batch.num_rows())
    }

    /// Convert the buffer to an Arrow RecordBatch.
    fn buffer_to_batch(&self) -> Result<Option<RecordBatch>, MayoError> {
        if self.buffer.is_empty() {
            return Ok(None);
        }

        let timestamps: Vec<u64> = self.buffer.iter().map(|s| s.timestamp).collect();
        let names: Vec<&str> = self.buffer.iter().map(|s| s.metric_name.as_str()).collect();
        let labels: Vec<&str> = self.buffer.iter().map(|s| s.labels_json.as_str()).collect();
        let values: Vec<f64> = self.buffer.iter().map(|s| s.value).collect();

        let batch = RecordBatch::try_new(
            Arc::new(metrics_schema()),
            vec![
                Arc::new(UInt64Array::from(timestamps)),
                Arc::new(StringArray::from(names)),
                Arc::new(StringArray::from(labels)),
                Arc::new(Float64Array::from(values)),
            ],
        )
        .map_err(|e| MayoError::Arrow(e.to_string()))?;

        Ok(Some(batch))
    }

    fn clear_completed_flush(&mut self) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.completed.load(std::sync::atomic::Ordering::Acquire))
        {
            self.pending = None;
        }
    }

    fn allocate_flush_target(&mut self) -> FlushTarget {
        let filename = format!("metrics_{:06}.parquet", self.flush_counter);
        self.flush_counter += 1;
        match &self.backend {
            Backend::Local => FlushTarget::Local {
                data_dir: self.data_dir.clone(),
                path: self.data_dir.join(filename),
            },
            Backend::Remote { store, prefix } => FlushTarget::Remote {
                store: store.clone(),
                key: prefix
                    .clone()
                    .join(format!("metrics_{:032x}.parquet", rand::random::<u128>()).as_str()),
            },
        }
    }

    /// Flush at most two batches: older pending rows, then the current buffer.
    /// Publication failure or cancellation preserves pending ownership; only
    /// confirmed completion permits its removal. This method borrows the store
    /// through I/O; use `flush_off_lock` when readers must remain concurrent.
    pub async fn flush(&mut self) -> Result<(), MayoError> {
        for _ in 0..2 {
            let Some(pending) = self.take_flush_batch()? else {
                break;
            };
            write_pending_flush(pending).await?;
            self.clear_completed_flush();
        }
        Ok(())
    }

    /// Return the retained pending batch or freeze and retain the current buffer.
    /// Clear only confirmed completion. A proved foreign collision changes the
    /// destination while preserving immutable rows and encoded bytes. The clone
    /// can be written off-lock; dropping it does not acknowledge publication.
    pub fn take_flush_batch(&mut self) -> Result<Option<PendingFlush>, MayoError> {
        self.clear_completed_flush();
        if let Some(previous) = self.pending.take_if(|pending| {
            pending
                .foreign_collision
                .load(std::sync::atomic::Ordering::Acquire)
        }) {
            // Retarget only a proved foreign collision while holding the store
            // exclusively; preserve immutable payload and directory ownership.
            let retargeted = PendingFlush {
                target: self.allocate_flush_target(),
                writing: Arc::new(tokio::sync::Mutex::new(())),
                completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                foreign_collision: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                ..previous
            };
            self.pending = Some(retargeted);
        }
        if let Some(pending) = &self.pending {
            return Ok(Some(pending.clone()));
        }
        let Some(batch) = self.buffer_to_batch()? else {
            return Ok(None);
        };
        let pending = PendingFlush {
            batch,
            target: self.allocate_flush_target(),
            publication: format!("{:032x}", rand::random::<u128>()),
            encoded: Arc::new(std::sync::Mutex::new(None)),
            local_parent_entries: Arc::new(std::sync::Mutex::new(None)),
            writing: Arc::new(tokio::sync::Mutex::new(())),
            completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            foreign_collision: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            test_gate: self.test_gate.clone(),
            #[cfg(test)]
            creation_confirmation_fault: self
                .test_gate
                .as_ref()
                .and_then(|gate| gate.creation_confirmation_fault.lock().unwrap().clone()),
        };
        self.buffer.clear();
        self.pending = Some(pending.clone());
        Ok(Some(pending))
    }

    /// Build a DataFusion session exposing a `metrics` table over all data:
    /// the Parquet files unioned with the unflushed buffer.
    async fn session(&self) -> Result<SessionContext, MayoError> {
        self.session_since(None).await
    }

    /// [`session`](Self::session) for a query whose SQL only wants samples at
    /// or after `since`.
    ///
    /// The `metrics` table is streamed (#377): this method lists file names
    /// and verifies only the exact retained publication, if present, so that
    /// its memory rows do not duplicate a committed archive chunk. Each query reads its files one at a time, skips
    /// files and row groups whose footer statistics rule out its time range
    /// or metric names, and decodes only the columns it uses. `since` is one
    /// more lower bound for that pruning; the SQL's own `timestamp`
    /// predicates already give the same, so it's a promise, not a filter.
    async fn session_since(&self, since: Option<u64>) -> Result<SessionContext, MayoError> {
        let ctx = streaming_session();
        let sources = match &self.backend {
            Backend::Local => list_local(&self.data_dir)?,
            Backend::Remote { store, prefix } => list_remote(store, prefix, "metrics").await?,
        };
        let mut memory = Vec::new();
        if let Some(batch) = self.buffer_to_batch()? {
            memory.push(batch);
        }
        let mut archived = Vec::new();
        if let Some(pending) = &self.pending {
            memory.push(pending.batch.clone());
            for source in sources {
                if pending.targets(&source) && pending.owns_source(&source).await? {
                    continue;
                }
                archived.push(source);
            }
        } else {
            archived = sources;
        }
        // The exact immutable source listing and retained batch are one query
        // snapshot; an atomic publication after listing cannot add a duplicate.
        let memory = if memory.is_empty() {
            None
        } else {
            Some(
                datafusion::arrow::compute::concat_batches(&Arc::new(metrics_schema()), &memory)
                    .map_err(|error| MayoError::Arrow(error.to_string()))?,
            )
        };
        let table = ParquetTable::new(
            Arc::new(metrics_schema()),
            archived,
            memory,
            since,
            "metrics",
        );
        ctx.register_table("metrics", Arc::new(table))
            .map_err(|e| MayoError::DataFusion(e.to_string()))?;
        Ok(ctx)
    }

    /// Query metrics using SQL. Returns (timestamp, name, labels, value) tuples.
    pub async fn query_sql(&self, sql: &str) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        self.query_rows(sql, None).await
    }

    /// [`query_sql`](Self::query_sql) for SQL that only selects samples with
    /// `timestamp >= since`. Files entirely older than `since` aren't read,
    /// so the SQL must filter on that bound itself or it would see a subset.
    pub async fn query_sql_since(
        &self,
        sql: &str,
        since: u64,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        self.query_rows(sql, Some(since)).await
    }

    async fn query_rows(
        &self,
        sql: &str,
        since: Option<u64>,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        let ctx = self.session_since(since).await?;
        let df = ctx
            .sql(sql)
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        let batches = df
            .collect()
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        let mut results = Vec::new();
        for batch in &batches {
            if batch.num_columns() < 4 {
                continue;
            }
            let timestamps = batch
                .column(0)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .ok_or_else(|| MayoError::Arrow("timestamp column type mismatch".into()))?;
            let names = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| MayoError::Arrow("metric_name column type mismatch".into()))?;
            let labels = batch
                .column(2)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| MayoError::Arrow("labels column type mismatch".into()))?;
            let values = batch
                .column(3)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| MayoError::Arrow("value column type mismatch".into()))?;

            for i in 0..batch.num_rows() {
                results.push((
                    timestamps.value(i),
                    names.value(i).to_string(),
                    labels.value(i).to_string(),
                    values.value(i),
                ));
            }
        }

        Ok(results)
    }

    /// One app's samples in `[start, end]`, oldest first, newest kept.
    ///
    /// `app_label` is matched against label values (the `namespace/app`
    /// every per-app sample carries) and `name`, when given, against the
    /// metric name. At most [`APP_QUERY_ROW_LIMIT`] rows come back, and they
    /// are the newest ones: a long window loses its oldest samples, never the
    /// latest. `per_series` keeps only the newest N samples of each series
    /// (metric name plus labels), which is how a caller asks for "the latest
    /// value of everything" without paying for the whole window.
    pub async fn query_app(
        &self,
        app_label: &str,
        name: Option<&str>,
        start: u64,
        end: u64,
        per_series: Option<u32>,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        let app_filter = escape_sql_literal(app_label);
        let name_filter = name
            .map(|name| format!("metric_name = '{}' AND ", escape_sql_literal(name)))
            .unwrap_or_default();
        let filter = format!(
            "{name_filter}labels LIKE '%\"{app_filter}\"%' \
             AND timestamp >= {start} AND timestamp <= {end}"
        );
        let sql = match per_series {
            None => format!(
                "SELECT timestamp, metric_name, labels, value FROM metrics \
                 WHERE {filter} ORDER BY timestamp DESC LIMIT {APP_QUERY_ROW_LIMIT}"
            ),
            Some(keep) => format!(
                "SELECT timestamp, metric_name, labels, value FROM ( \
                   SELECT timestamp, metric_name, labels, value, \
                     ROW_NUMBER() OVER ( \
                       PARTITION BY metric_name, labels ORDER BY timestamp DESC \
                     ) AS series_rank \
                   FROM metrics WHERE {filter} \
                 ) WHERE series_rank <= {keep} \
                 ORDER BY timestamp DESC LIMIT {APP_QUERY_ROW_LIMIT}"
            ),
        };
        let mut rows = self.query_sql_since(&sql, start).await?;
        rows.reverse();
        Ok(rows)
    }

    /// Query by metric name and time range (convenience).
    pub async fn query(
        &self,
        metric_name: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        let metric_name = escape_sql_literal(metric_name);
        let sql = format!(
            "SELECT timestamp, metric_name, labels, value FROM metrics \
             WHERE metric_name = '{metric_name}' \
             AND timestamp >= {start} AND timestamp <= {end} \
             ORDER BY timestamp"
        );
        self.query_sql_since(&sql, start).await
    }

    /// Every series' samples in `[start, end]`, oldest first, at most
    /// [`ALL_QUERY_ROW_LIMIT`] of them: what `/v1/metrics?name=*` returns.
    pub async fn query_all(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        let sql = format!(
            "SELECT timestamp, metric_name, labels, value FROM metrics \
             WHERE timestamp >= {start} AND timestamp <= {end} \
             ORDER BY timestamp LIMIT {ALL_QUERY_ROW_LIMIT}"
        );
        self.query_sql_since(&sql, start).await
    }

    /// Samples of the named metrics at or after `since`, oldest first. `relish
    /// top` reads its CPU and memory columns this way.
    pub async fn query_names_since(
        &self,
        names: &[&str],
        since: u64,
    ) -> Result<Vec<(u64, String, String, f64)>, MayoError> {
        let names = names
            .iter()
            .map(|name| format!("'{}'", escape_sql_literal(name)))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT timestamp, metric_name, labels, value FROM metrics \
             WHERE metric_name IN ({names}) \
             AND timestamp >= {since} ORDER BY timestamp"
        );
        self.query_sql_since(&sql, since).await
    }

    /// Query the average value of a metric over a time window.
    ///
    /// Used by the autoscaler to compute average CPU/memory utilisation.
    /// The `app_label` filters by the `app` label in the metrics labels JSON.
    /// Returns `None` if no data points exist in the window.
    pub async fn query_avg(
        &self,
        metric_name: &str,
        app_label: &str,
        window_secs: u64,
    ) -> Result<Option<f64>, MayoError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let start = now.saturating_sub(window_secs);

        let metric_name = escape_sql_literal(metric_name);
        let app_label = escape_sql_literal(app_label);
        let sql = format!(
            "SELECT AVG(value) as avg_val FROM metrics \
             WHERE metric_name = '{metric_name}' \
             AND labels LIKE '%\"{app_label}\"%' \
             AND timestamp >= {start} AND timestamp <= {now}"
        );

        let ctx = self.session_since(Some(start)).await?;
        let df = ctx
            .sql(&sql)
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        let batches = df
            .collect()
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        for batch in &batches {
            if batch.num_rows() == 0 || batch.num_columns() == 0 {
                continue;
            }
            if let Some(col) = batch.column(0).as_any().downcast_ref::<Float64Array>()
                && !col.is_null(0)
            {
                return Ok(Some(col.value(0)));
            }
        }

        Ok(None)
    }

    /// List all distinct metric names.
    pub async fn metric_names(&self) -> Result<Vec<String>, MayoError> {
        let ctx = self.session().await?;
        let df = ctx
            .sql("SELECT DISTINCT metric_name FROM metrics ORDER BY metric_name")
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        let batches = df
            .collect()
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        let mut names = Vec::new();
        for batch in &batches {
            let col = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| MayoError::Arrow("column type mismatch".into()))?;
            for i in 0..batch.num_rows() {
                names.push(col.value(i).to_string());
            }
        }

        Ok(names)
    }

    /// Query aggregated statistics for all metrics in a time window.
    ///
    /// Returns (metric_name, labels_json, min, max, sum, count) tuples,
    /// one per distinct (metric_name, labels) combination. Used by the
    /// rollup generator to build `NodeRollup` entries.
    pub async fn query_window_aggregates(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Vec<(String, String, f64, f64, f64, u32)>, MayoError> {
        let sql = format!(
            "SELECT metric_name, labels, \
             MIN(value) as min_val, MAX(value) as max_val, \
             SUM(value) as sum_val, COUNT(*) as count_val \
             FROM metrics \
             WHERE timestamp >= {start} AND timestamp < {end} \
             GROUP BY metric_name, labels \
             ORDER BY metric_name, labels"
        );

        let ctx = self.session_since(Some(start)).await?;
        let df = ctx
            .sql(&sql)
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        let batches = df
            .collect()
            .await
            .map_err(|e| MayoError::QueryFailed(e.to_string()))?;

        let mut results = Vec::new();
        for batch in &batches {
            let names = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| MayoError::Arrow("metric_name column type mismatch".into()))?;
            let labels = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| MayoError::Arrow("labels column type mismatch".into()))?;
            let mins = batch
                .column(2)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| MayoError::Arrow("min column type mismatch".into()))?;
            let maxs = batch
                .column(3)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| MayoError::Arrow("max column type mismatch".into()))?;
            let sums = batch
                .column(4)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| MayoError::Arrow("sum column type mismatch".into()))?;
            // COUNT(*) returns i64 in DataFusion
            let counts = batch
                .column(5)
                .as_any()
                .downcast_ref::<datafusion::arrow::array::Int64Array>()
                .ok_or_else(|| MayoError::Arrow("count column type mismatch".into()))?;

            for i in 0..batch.num_rows() {
                results.push((
                    names.value(i).to_string(),
                    labels.value(i).to_string(),
                    mins.value(i),
                    maxs.value(i),
                    sums.value(i),
                    counts.value(i) as u32,
                ));
            }
        }

        Ok(results)
    }

    /// Prune Parquet files whose newest datapoint is older than `before`.
    ///
    /// Retention is keyed on the data's own newest timestamp (O12), read from
    /// the file's row-group statistics — not the file's mtime, which a
    /// touch/copy or clock skew can push forward and so drop in-range data. A
    /// file whose max timestamp can't be read falls back to mtime so a
    /// malformed file is still eligible for pruning.
    pub fn prune(&self, before: u64) -> Result<usize, MayoError> {
        // Remote backends leave retention to the bucket's own lifecycle policy
        // (the idiomatic way to expire object-store data), so node-side pruning
        // is a no-op there (H8).
        if let Backend::Remote { .. } = &self.backend {
            return Ok(0);
        }
        let mut deleted = 0;
        let entries = match std::fs::read_dir(&self.data_dir) {
            Ok(entries) => entries,
            // A new store has no directory until its first flush.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(MayoError::Io(std::io::Error::new(
                    error.kind(),
                    format!("list metrics {}: {error}", self.data_dir.display()),
                )));
            }
        };
        for entry in entries {
            let path = entry
                .map_err(|error| {
                    MayoError::Io(std::io::Error::new(
                        error.kind(),
                        format!(
                            "read metrics directory {}: {error}",
                            self.data_dir.display()
                        ),
                    ))
                })?
                .path();
            if !path.extension().is_some_and(|e| e == "parquet") {
                continue;
            }
            if self.pending.as_ref().is_some_and(|pending| {
                matches!(&pending.target, FlushTarget::Local { path: owned, .. } if owned == &path)
            }) { continue; }
            let newest = file_max_timestamp(&path).unwrap_or_else(|| {
                std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|t| {
                        t.duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                    })
                    .unwrap_or(u64::MAX)
            });
            if newest < before {
                match std::fs::remove_file(&path) {
                    Ok(()) => deleted += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(MayoError::Io(std::io::Error::new(
                            error.kind(),
                            format!("prune {}: {error}", path.display()),
                        )));
                    }
                }
            }
        }
        Ok(deleted)
    }
}

/// The maximum `timestamp` (column 0) across a Parquet file's row groups, read
/// from statistics without scanning the data. `None` if the file is unreadable
/// or carries no usable stats.
pub(crate) fn file_max_timestamp(path: &Path) -> Option<u64> {
    use datafusion::parquet::file::reader::{FileReader, SerializedFileReader};
    use datafusion::parquet::file::statistics::Statistics;

    let reader = SerializedFileReader::new(std::fs::File::open(path).ok()?).ok()?;
    let meta = reader.metadata();
    let mut max: Option<u64> = None;
    for i in 0..meta.num_row_groups() {
        let rg = meta.row_group(i);
        if rg.num_columns() == 0 {
            continue;
        }
        let candidate = match rg.column(0).statistics() {
            Some(Statistics::Int64(s)) => s.max_opt().map(|v| *v as u64),
            Some(Statistics::Int32(s)) => s.max_opt().map(|v| *v as u64),
            _ => None,
        };
        if let Some(c) = candidate {
            max = Some(max.map_or(c, |cur| cur.max(c)));
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mayo::types::MetricKey;

    #[test]
    fn prune_reports_failed_deletions_and_preserves_survivors() {
        let (store, dir) = test_store();
        let blocked = dir.path().join("blocked.parquet");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("survivor"), b"preserve").unwrap();
        let error = store
            .prune(u64::MAX)
            .expect_err("failed removal must not count as deletion");
        assert!(error.to_string().contains("blocked.parquet"), "{error}");
        assert_eq!(
            std::fs::read(blocked.join("survivor")).unwrap(),
            b"preserve"
        );
    }

    fn app_key(name: &str, app: &str, instance: &str) -> MetricKey {
        MetricKey::with_labels(
            name,
            std::collections::BTreeMap::from([
                ("app".to_string(), app.to_string()),
                ("instance".to_string(), instance.to_string()),
            ]),
        )
    }

    /// The old per-app query ordered ascending from `start` and cut at the
    /// row limit, so a busy app's newest samples, the ones every "latest"
    /// view wants, were the first thing dropped.
    #[tokio::test]
    async fn app_query_keeps_the_newest_rows_when_over_the_limit() {
        let (mut store, _dir) = test_store();
        let key = app_key("requests_total", "default/web", "web-0");
        let total = APP_QUERY_ROW_LIMIT as u64 + 500;
        for timestamp in 1..=total {
            store.insert(&key, Sample::at(timestamp, timestamp as f64));
        }
        let rows = store
            .query_app("default/web", Some("requests_total"), 0, total, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), APP_QUERY_ROW_LIMIT);
        assert_eq!(rows.last().unwrap().0, total, "the newest sample was lost");
        assert!(
            rows.windows(2).all(|pair| pair[0].0 <= pair[1].0),
            "rows must come back oldest first"
        );
    }

    #[tokio::test]
    async fn app_query_per_series_keeps_the_newest_n_of_each_series() {
        let (mut store, _dir) = test_store();
        for instance in ["web-0", "web-1"] {
            let key = app_key("requests_total", "default/web", instance);
            for timestamp in 1..=5 {
                store.insert(&key, Sample::at(timestamp, timestamp as f64));
            }
        }
        store.insert(
            &app_key("requests_total", "default/other", "other-0"),
            Sample::at(5, 99.0),
        );
        let rows = store
            .query_app("default/web", None, 0, 10, Some(2))
            .await
            .unwrap();
        let mut seen: Vec<(u64, String)> = rows
            .iter()
            .map(|(timestamp, _, labels, _)| (*timestamp, labels.clone()))
            .collect();
        seen.sort();
        assert_eq!(rows.len(), 4, "{seen:?}");
        assert!(rows.iter().all(|(timestamp, ..)| *timestamp >= 4));
        assert!(
            rows.iter()
                .all(|(_, _, labels, _)| !labels.contains("other"))
        );
    }

    #[tokio::test]
    async fn app_query_escapes_the_app_and_name() {
        let (mut store, _dir) = test_store();
        store.insert(&app_key("m", "default/web", "web-0"), Sample::at(1, 1.0));
        let rows = store
            .query_app("x' OR '1'='1", Some("m' OR '1'='1"), 0, 10, Some(1))
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    fn test_store() -> (MayoStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = MayoStore::new(dir.path().to_path_buf());
        (store, dir)
    }

    /// H8: with an object-store backend (exercised here via a `file://` temp
    /// dir — object_store's LocalFileSystem — so no real cloud is needed),
    /// flushed metrics land in the store and the same queries read them back,
    /// including across a "restart" that re-opens the store and resumes the
    /// flush counter from what's already there.
    #[tokio::test]
    async fn object_store_backend_round_trips_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let url = url::Url::from_file_path(dir.path()).unwrap().to_string();

        let mut store = MayoStore::open(dir.path().to_path_buf(), Some(&url))
            .await
            .unwrap();
        let key = MetricKey::simple("node_cpu");
        store.insert(&key, Sample::at(1000, 10.0));
        store.insert(&key, Sample::at(1001, 20.0));
        store.flush().await.unwrap();

        let rows = store.query("node_cpu", 0, 2000).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].3, 10.0);

        // Re-open (restart): the store lists existing objects, so a fresh flush
        // doesn't clobber the first file, and both are queryable.
        let mut restarted = MayoStore::open(dir.path().to_path_buf(), Some(&url))
            .await
            .unwrap();
        restarted.insert(&key, Sample::at(1002, 30.0));
        restarted.flush().await.unwrap();
        let rows = restarted.query("node_cpu", 0, 2000).await.unwrap();
        assert_eq!(rows.len(), 3, "restart must not clobber earlier objects");

        // Prune is a no-op for a remote backend (bucket lifecycle owns retention).
        assert_eq!(restarted.prune(u64::MAX).unwrap(), 0);
    }

    #[tokio::test]
    async fn reserving_a_flush_retains_queryable_samples_until_persistence() {
        let (mut store, _directory) = test_store();
        store.insert(&MetricKey::simple("cpu"), Sample::at(1, 1.0));
        store.insert(&MetricKey::simple("cpu"), Sample::at(2, 2.0));
        let pending = store.take_flush_batch().unwrap().unwrap();
        assert_eq!(store.buffer_len(), 2);
        assert_eq!(store.query("cpu", 0, 10).await.unwrap().len(), 2);
        write_pending_flush(pending).await.unwrap();
        assert_eq!(store.query("cpu", 0, 10).await.unwrap().len(), 2);
        assert_eq!(store.buffer_len(), 0);
        store.flush().await.unwrap();
        assert_eq!(store.query("cpu", 0, 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn insert_and_flush_creates_parquet() {
        let (mut store, dir) = test_store();
        let key = MetricKey::simple("cpu_usage");
        store.insert(&key, Sample::at(1000, 42.5));
        store.insert(&key, Sample::at(1001, 43.0));

        store.flush().await.unwrap();

        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "parquet"))
            .collect();
        assert_eq!(files.len(), 1);
    }

    /// O12: retention keys on the data's own newest timestamp, not the file's
    /// mtime. A file just written (recent mtime) but holding only old data is
    /// pruned; a file holding recent data is kept regardless of mtime.
    #[tokio::test]
    async fn prune_uses_data_timestamp_not_file_mtime() {
        let (mut store, dir) = test_store();
        // Freshly written file (mtime ~now), but its newest datapoint is ts=5.
        store.insert(&MetricKey::simple("cpu"), Sample::at(5, 1.0));
        store.flush().await.unwrap();

        // mtime-based pruning would keep this (mtime is now); timestamp-based
        // prunes it (data max 5 < 100).
        assert_eq!(store.prune(100).unwrap(), 1);
        assert!(!dir_has_parquet(dir.path()));

        // A file with recent data survives the same cutoff.
        store.insert(&MetricKey::simple("cpu"), Sample::at(1_000, 2.0));
        store.flush().await.unwrap();
        assert_eq!(store.prune(100).unwrap(), 0);
        assert!(dir_has_parquet(dir.path()));
    }

    #[tokio::test]
    async fn query_after_flush() {
        let (mut store, _dir) = test_store();
        let key = MetricKey::simple("cpu_usage");
        store.insert(&key, Sample::at(1000, 42.5));
        store.insert(&key, Sample::at(1001, 43.0));
        store.insert(&key, Sample::at(1002, 44.0));
        store.flush().await.unwrap();

        let results = store.query("cpu_usage", 1000, 1002).await.unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, 1000);
        assert_eq!(results[0].3, 42.5);
    }

    #[tokio::test]
    async fn query_time_range_filters() {
        let (mut store, _dir) = test_store();
        let key = MetricKey::simple("mem");
        store.insert(&key, Sample::at(100, 1.0));
        store.insert(&key, Sample::at(200, 2.0));
        store.insert(&key, Sample::at(300, 3.0));
        store.flush().await.unwrap();

        let results = store.query("mem", 150, 250).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].3, 2.0);
    }

    /// #310: a windowed read skips files wholly before its window, but a file
    /// whose newest sample sits exactly on the window's start still counts,
    /// and so does one that only straddles it.
    #[tokio::test]
    async fn windowed_reads_keep_every_file_that_reaches_the_window() {
        let (mut store, _dir) = test_store();
        let key = MetricKey::simple("mem");
        for file in [[100, 150], [180, 200], [190, 260]] {
            for timestamp in file {
                store.insert(&key, Sample::at(timestamp, timestamp as f64));
            }
            store.flush().await.unwrap();
        }
        store.insert(&key, Sample::at(300, 300.0));

        let rows = store.query("mem", 200, 400).await.unwrap();
        let seen: Vec<u64> = rows.iter().map(|row| row.0).collect();
        assert_eq!(seen, vec![200, 260, 300]);
        let since = store
            .query_sql_since(
                "SELECT timestamp, metric_name, labels, value FROM metrics \
                 WHERE timestamp >= 150 ORDER BY timestamp",
                150,
            )
            .await
            .unwrap();
        let seen: Vec<u64> = since.iter().map(|row| row.0).collect();
        assert_eq!(seen, vec![150, 180, 190, 200, 260, 300]);
    }

    #[tokio::test]
    async fn query_nonexistent_metric_returns_empty() {
        let (mut store, _dir) = test_store();
        let key = MetricKey::simple("cpu");
        store.insert(&key, Sample::at(1000, 1.0));
        store.flush().await.unwrap();

        let results = store.query("nonexistent", 0, 9999).await.unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn escape_sql_literal_doubles_quotes() {
        assert_eq!(escape_sql_literal("cpu_usage"), "cpu_usage");
        assert_eq!(escape_sql_literal("a'b"), "a''b");
        assert_eq!(escape_sql_literal("x' OR '1'='1"), "x'' OR ''1''=''1");
    }

    /// M1: an injection payload in the metric name must not break out of
    /// the SQL literal and leak another metric's rows.
    #[tokio::test]
    async fn query_metric_name_injection_is_neutralised() {
        let (mut store, _dir) = test_store();
        store.insert(&MetricKey::simple("secret"), Sample::at(1000, 9.9));
        store.flush().await.unwrap();

        // Classic `' OR '1'='1` — if unescaped it would return every row.
        let results = store.query("x' OR '1'='1", 0, 9999).await.unwrap();
        assert!(results.is_empty(), "SQL injection leaked rows: {results:?}");
    }

    #[tokio::test]
    async fn query_with_labels() {
        let (mut store, _dir) = test_store();
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("app".to_string(), "web".to_string());
        let key = MetricKey::with_labels("requests", labels);
        store.insert(&key, Sample::at(1000, 100.0));
        store.flush().await.unwrap();

        let results = store
            .query_sql(
                "SELECT timestamp, metric_name, labels, value FROM metrics \
                 WHERE metric_name = 'requests' AND labels LIKE '%web%'",
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].3, 100.0);
    }

    #[tokio::test]
    async fn multiple_metrics_in_same_store() {
        let (mut store, _dir) = test_store();
        store.insert(&MetricKey::simple("cpu"), Sample::at(1000, 50.0));
        store.insert(&MetricKey::simple("mem"), Sample::at(1000, 1024.0));
        store.flush().await.unwrap();

        let cpu = store.query("cpu", 0, 9999).await.unwrap();
        let mem = store.query("mem", 0, 9999).await.unwrap();
        assert_eq!(cpu.len(), 1);
        assert_eq!(mem.len(), 1);
        assert_eq!(cpu[0].3, 50.0);
        assert_eq!(mem[0].3, 1024.0);
    }

    #[tokio::test]
    async fn metric_names_lists_distinct() {
        let (mut store, _dir) = test_store();
        store.insert(&MetricKey::simple("beta"), Sample::at(1, 1.0));
        store.insert(&MetricKey::simple("alpha"), Sample::at(1, 2.0));
        store.insert(&MetricKey::simple("beta"), Sample::at(2, 3.0));
        store.flush().await.unwrap();

        let names = store.metric_names().await.unwrap();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[tokio::test]
    async fn flush_empty_buffer_is_noop() {
        let (mut store, _dir) = test_store();
        store.flush().await.unwrap();
    }

    #[tokio::test]
    async fn buffer_len_tracks_inserts() {
        let (mut store, _dir) = test_store();
        assert_eq!(store.buffer_len(), 0);
        store.insert(&MetricKey::simple("x"), Sample::at(1, 1.0));
        assert_eq!(store.buffer_len(), 1);
    }

    #[tokio::test]
    async fn flush_clears_buffer() {
        let (mut store, _dir) = test_store();
        store.insert(&MetricKey::simple("x"), Sample::at(1, 1.0));
        store.flush().await.unwrap();
        assert_eq!(store.buffer_len(), 0);
    }

    #[tokio::test]
    async fn multiple_flushes_queryable() {
        let (mut store, _dir) = test_store();
        store.insert(&MetricKey::simple("a"), Sample::at(1, 1.0));
        store.flush().await.unwrap();

        store.insert(&MetricKey::simple("b"), Sample::at(2, 2.0));
        store.flush().await.unwrap();

        let names = store.metric_names().await.unwrap();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn arrow_schema_has_expected_columns() {
        let schema = metrics_schema();
        assert_eq!(schema.fields().len(), 4);
        assert_eq!(schema.field(0).name(), "timestamp");
        assert_eq!(schema.field(1).name(), "metric_name");
        assert_eq!(schema.field(2).name(), "labels");
        assert_eq!(schema.field(3).name(), "value");
    }

    #[tokio::test]
    async fn query_empty_store_returns_empty() {
        let (store, _dir) = test_store();
        let results = store.query("anything", 0, 9999).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn query_unflushed_buffer_visible() {
        let (mut store, _dir) = test_store();
        let key = MetricKey::simple("live_metric");
        store.insert(&key, Sample::at(1000, 42.0));
        // Don't flush — query should still see buffer data
        let results = store.query("live_metric", 0, 9999).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].3, 42.0);
    }

    #[tokio::test]
    async fn reopen_reads_persisted_parquet_without_clobbering() {
        let dir = tempfile::tempdir().unwrap();

        // First run: flush two separate files.
        {
            let mut store = MayoStore::new(dir.path().to_path_buf());
            store.insert(&MetricKey::simple("cpu"), Sample::at(1, 10.0));
            store.flush().await.unwrap();
            store.insert(&MetricKey::simple("cpu"), Sample::at(2, 20.0));
            store.flush().await.unwrap();
        }
        let files_after_first = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "parquet")
            })
            .count();
        assert_eq!(files_after_first, 2);

        // Second run over the same dir: prior data is queryable...
        let mut store = MayoStore::new(dir.path().to_path_buf());
        let results = store.query("cpu", 0, 9999).await.unwrap();
        assert_eq!(
            results.len(),
            2,
            "persisted data not reloaded after restart"
        );

        // ...and a new flush appends a third file, not overwriting file 000000.
        store.insert(&MetricKey::simple("cpu"), Sample::at(3, 30.0));
        store.flush().await.unwrap();
        let files_after_second = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "parquet")
            })
            .count();
        assert_eq!(files_after_second, 3, "restart clobbered an existing file");

        let all = store.query("cpu", 0, 9999).await.unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn query_sees_both_flushed_and_unflushed() {
        let (mut store, _dir) = test_store();
        store.insert(&MetricKey::simple("m"), Sample::at(1, 10.0));
        store.flush().await.unwrap();

        store.insert(&MetricKey::simple("m"), Sample::at(2, 20.0));
        // Second sample not flushed

        let results = store.query("m", 0, 9999).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].3, 10.0);
        assert_eq!(results[1].3, 20.0);
    }

    #[tokio::test]
    async fn query_proceeds_during_flush() {
        // OBS5: the flush I/O must not hold the store lock. We drain the buffer
        // under a brief write lock, release it, then run the (blocking) write
        // and a concurrent read at the same time. If the write held the lock,
        // the read would block until it finished; because it doesn't, both
        // complete together. No sleep — `join!` drives both to completion and
        // the read asserting the flushed row proves it observed a consistent
        // store while the write was in flight.
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(RwLock::new(MayoStore::new(dir.path().to_path_buf())));

        // Seed and flush one row so a query has something to read.
        {
            let mut s = store.write().await;
            s.insert(&MetricKey::simple("m"), Sample::at(1, 10.0));
            let pending = s.take_flush_batch().unwrap().unwrap();
            drop(s); // lock released before the write
            write_pending_flush(pending).await.unwrap();
        }

        // Now stage a second flush and run its write concurrently with a query.
        let pending = {
            let mut s = store.write().await;
            s.insert(&MetricKey::simple("m"), Sample::at(2, 20.0));
            s.take_flush_batch().unwrap().unwrap()
        }; // write lock dropped here — the write below holds no store lock

        let read_store = Arc::clone(&store);
        let (write_res, read_res) = tokio::join!(write_pending_flush(pending), async move {
            let s = read_store.read().await;
            s.query("m", 0, 9999).await
        });
        write_res.unwrap();
        // The read ran against the store while the flush write was in flight and
        // returned the already-persisted first row without blocking.
        let rows = read_res.unwrap();
        assert!(
            rows.iter().any(|r| r.3 == 10.0),
            "concurrent query did not see persisted data: {rows:?}"
        );
    }

    #[tokio::test]
    async fn corrupt_parquet_file_does_not_fail_query() {
        // OBS5: a truncated/garbage Parquet file must be skipped on read, not
        // fail an unrelated query.
        let (mut store, dir) = test_store();
        store.insert(&MetricKey::simple("cpu"), Sample::at(1, 10.0));
        store.flush().await.unwrap();

        std::fs::write(dir.path().join("metrics_999999.parquet"), b"garbage").unwrap();

        let results = store.query("cpu", 0, 9999).await.unwrap();
        assert_eq!(results.len(), 1, "corrupt file broke an unrelated query");
        assert_eq!(results[0].3, 10.0);
    }

    #[tokio::test]
    async fn flush_off_lock_writes_and_reports_emptiness() {
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(RwLock::new(MayoStore::new(dir.path().to_path_buf())));

        // Empty buffer: nothing written, reports false.
        assert!(!flush_off_lock(&store).await.unwrap());

        // With data: writes one Parquet file, reports true, clears the buffer.
        store
            .write()
            .await
            .insert(&MetricKey::simple("cpu"), Sample::at(1, 7.0));
        assert!(flush_off_lock(&store).await.unwrap());
        assert_eq!(store.read().await.buffer_len(), 0);

        let files = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "parquet")
            })
            .count();
        assert_eq!(files, 1);

        // The data is queryable afterwards.
        let rows = store.read().await.query("cpu", 0, 9999).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].3, 7.0);
    }

    #[tokio::test]
    async fn prune_removes_old_parquet_files() {
        let (mut store, dir) = test_store();
        store.insert(&MetricKey::simple("cpu"), Sample::at(1, 10.0));
        store.flush().await.unwrap();
        assert!(dir_has_parquet(dir.path()));

        // A `before` far in the future prunes every file (they're older).
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 10_000;
        let deleted = store.prune(future).unwrap();
        assert_eq!(deleted, 1);
        assert!(!dir_has_parquet(dir.path()));
    }

    #[tokio::test]
    async fn prune_keeps_recent_parquet_files() {
        let (mut store, dir) = test_store();
        store.insert(&MetricKey::simple("cpu"), Sample::at(1, 10.0));
        store.flush().await.unwrap();

        // A `before` of 0 keeps everything (nothing is older than the epoch).
        let deleted = store.prune(0).unwrap();
        assert_eq!(deleted, 0);
        assert!(dir_has_parquet(dir.path()));
    }

    #[tokio::test]
    async fn query_avg_filters_by_app_label_and_window() {
        let (mut store, _dir) = test_store();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut web = std::collections::BTreeMap::new();
        web.insert("app".to_string(), "web".to_string());
        let web_key = MetricKey::with_labels("cpu", web);
        store.insert(&web_key, Sample::at(now - 5, 10.0));
        store.insert(&web_key, Sample::at(now - 4, 30.0));

        let mut other = std::collections::BTreeMap::new();
        other.insert("app".to_string(), "other".to_string());
        let other_key = MetricKey::with_labels("cpu", other);
        store.insert(&other_key, Sample::at(now - 5, 100.0));
        store.flush().await.unwrap();

        // Average across web's two samples only: (10 + 30) / 2 = 20.
        let avg = store.query_avg("cpu", "web", 60).await.unwrap();
        assert_eq!(avg, Some(20.0));

        // No data for an unknown app in the window → None.
        let none = store.query_avg("cpu", "ghost", 60).await.unwrap();
        assert_eq!(none, None);
    }

    /// Rows in a canonical order, one per line, so a snapshot compares sets
    /// whatever order files were listed in. The caller checks the ordering
    /// it was promised separately.
    fn canonical(rows: &[(u64, String, String, f64)]) -> String {
        let mut rows = rows.to_vec();
        rows.sort_by(|left, right| {
            (left.0, &left.1, &left.2)
                .cmp(&(right.0, &right.1, &right.2))
                .then(left.3.total_cmp(&right.3))
        });
        rows.iter()
            .map(|(timestamp, name, labels, value)| format!("{timestamp} {name} {labels} {value}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn ascending(rows: &[(u64, String, String, f64)]) -> bool {
        rows.windows(2).all(|pair| pair[0].0 <= pair[1].0)
    }

    /// Five flushed files that overlap in time and differ in which metrics
    /// they hold, one corrupt file, and an unflushed buffer: the shapes the
    /// statistics pruning has to get right. Timestamps never repeat, so
    /// every ordered answer has exactly one right order.
    async fn golden_store(store: &mut MayoStore, corrupt: Option<&Path>) {
        let series = [
            app_key("cpu", "default/web", "web-0"),
            app_key("cpu", "default/api", "api-0"),
            app_key("mem", "default/web", "web-0"),
            app_key("requests_total", "other/web", "web-0"),
        ];
        // (first timestamp, which series) per file; the last entry stays in
        // the buffer. Every first timestamp and step is a multiple of 50 and
        // each file owns its own slots below 50, so no two rows collide.
        let files: [(u64, &[usize]); 6] = [
            (10_000, &[0, 1, 2]),
            (10_300, &[0, 2]),
            (10_100, &[3]),
            (10_600, &[1, 3]),
            (10_450, &[0, 1, 2, 3]),
            (10_900, &[0, 3]),
        ];
        for (index, (first, which)) in files.iter().enumerate() {
            for step in 0..12 {
                for (position, series_index) in which.iter().enumerate() {
                    let slot = (index * 5 + position) as u64;
                    let timestamp = first + step * 50 + slot;
                    let value = (timestamp % 13) as f64 + *series_index as f64 / 4.0;
                    store.insert(&series[*series_index], Sample::at(timestamp, value));
                }
            }
            if index + 1 < files.len() {
                store.flush().await.unwrap();
            }
        }
        if let Some(directory) = corrupt {
            std::fs::write(directory.join("metrics_999999.parquet"), b"not parquet").unwrap();
        }
    }

    /// Every read the store offers, over the golden data, as one report.
    async fn golden_report(store: &MayoStore) -> String {
        let unbounded = i64::MAX as u64;
        let mut report = Vec::new();
        let mut ordered = |label: &str, rows: Vec<(u64, String, String, f64)>| {
            assert!(ascending(&rows), "{label} came back out of order");
            report.push(format!(
                "## {label} ({} rows)\n{}",
                rows.len(),
                canonical(&rows)
            ));
        };
        ordered(
            "query cpu unbounded",
            store.query("cpu", 0, unbounded).await.unwrap(),
        );
        ordered(
            "query cpu window",
            store.query("cpu", 10_200, 10_700).await.unwrap(),
        );
        ordered(
            "query mem one instant",
            store.query("mem", 10_002, 10_002).await.unwrap(),
        );
        ordered(
            "query missing",
            store.query("missing", 0, unbounded).await.unwrap(),
        );
        ordered(
            "query_all unbounded",
            store.query_all(0, unbounded).await.unwrap(),
        );
        ordered(
            "query_all window",
            store.query_all(10_250, 10_800).await.unwrap(),
        );
        ordered(
            "query_app web",
            store
                .query_app("default/web", None, 0, unbounded, None)
                .await
                .unwrap(),
        );
        ordered(
            "query_app web cpu newest two",
            store
                .query_app("default/web", Some("cpu"), 10_000, 11_000, Some(2))
                .await
                .unwrap(),
        );
        ordered(
            "query_names_since cpu mem",
            store
                .query_names_since(&["cpu", "mem"], 10_500)
                .await
                .unwrap(),
        );
        let alert = store
            .query_sql_since(
                "SELECT timestamp, metric_name, labels, value FROM metrics \
                 WHERE timestamp >= 10800 ORDER BY timestamp DESC",
                10_800,
            )
            .await
            .unwrap();
        assert!(alert.windows(2).all(|pair| pair[0].0 >= pair[1].0));
        report.push(format!(
            "## alert read ({} rows)\n{}",
            alert.len(),
            canonical(&alert)
        ));
        let unordered = store
            .query_sql("SELECT timestamp, metric_name, labels, value FROM metrics")
            .await
            .unwrap();
        report.push(format!(
            "## every row ({} rows)\n{}",
            unordered.len(),
            canonical(&unordered)
        ));

        let aggregates = store.query_window_aggregates(10_300, 10_900).await.unwrap();
        report.push(format!(
            "## window aggregates\n{}",
            aggregates
                .iter()
                .map(|row| format!("{row:?}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        report.push(format!(
            "## metric names\n{:?}",
            store.metric_names().await.unwrap()
        ));
        report.join("\n\n")
    }

    /// #377: streaming must return exactly what loading every file returned.
    /// The snapshot was recorded from the eager implementation before the
    /// change, and both backends must still match it.
    #[tokio::test]
    async fn every_read_matches_the_eager_golden_answers() {
        let (mut local, directory) = test_store();
        golden_store(&mut local, Some(directory.path())).await;
        let local_report = golden_report(&local).await;
        insta::assert_snapshot!("mayo_golden_reads", local_report);

        let bucket = tempfile::tempdir().unwrap();
        let url = url::Url::from_file_path(bucket.path()).unwrap().to_string();
        let mut remote = MayoStore::open(bucket.path().to_path_buf(), Some(&url))
            .await
            .unwrap();
        golden_store(&mut remote, Some(bucket.path())).await;
        assert_eq!(golden_report(&remote).await, local_report);
    }

    /// The eager reader this store used before #377: every file decoded in
    /// full into one in-memory table with the buffer, then queried.
    async fn eager_rows(store: &MayoStore, sql: &str) -> Vec<(u64, String, String, f64)> {
        use datafusion::datasource::MemTable;
        use datafusion::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

        let schema = Arc::new(metrics_schema());
        let mut batches = Vec::new();
        for entry in std::fs::read_dir(&store.data_dir).unwrap().flatten() {
            let path = entry.path();
            if !path.extension().is_some_and(|x| x == "parquet") {
                continue;
            }
            let Ok(builder) =
                ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&path).unwrap())
            else {
                continue;
            };
            for batch in builder.build().unwrap() {
                let batch = batch.unwrap();
                batches
                    .push(RecordBatch::try_new(schema.clone(), batch.columns().to_vec()).unwrap());
            }
        }
        batches.extend(store.buffer_to_batch().unwrap());
        batches.push(RecordBatch::new_empty(schema.clone()));
        let ctx = SessionContext::new();
        ctx.register_table(
            "metrics",
            Arc::new(MemTable::try_new(schema, vec![batches]).unwrap()),
        )
        .unwrap();
        let mut rows = Vec::new();
        for batch in ctx.sql(sql).await.unwrap().collect().await.unwrap() {
            let timestamps = batch
                .column(0)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap();
            let names = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            let labels = batch
                .column(2)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            let values = batch
                .column(3)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();
            for i in 0..batch.num_rows() {
                rows.push((
                    timestamps.value(i),
                    names.value(i).to_string(),
                    labels.value(i).to_string(),
                    values.value(i),
                ));
            }
        }
        rows
    }

    /// The query shapes the pruning has to survive: ranges, strict bounds,
    /// names, IN lists, disjunctions, negations and an aggregate.
    fn property_sql(shape: usize, a: u64, b: u64, name: &str, other: &str) -> String {
        let select = "SELECT timestamp, metric_name, labels, value FROM metrics";
        match shape {
            0 => format!("{select} WHERE timestamp >= {a} AND timestamp <= {b}"),
            1 => format!(
                "{select} WHERE metric_name = '{name}' AND timestamp > {a} AND timestamp < {b}"
            ),
            2 => format!(
                "{select} WHERE metric_name IN ('{name}', '{other}') \
                 AND timestamp BETWEEN {a} AND {b}"
            ),
            3 => format!("{select} WHERE timestamp < {a} OR metric_name = '{name}'"),
            4 => format!(
                "{select} WHERE (metric_name = '{name}' AND timestamp <= {a}) \
                 OR timestamp >= {b}"
            ),
            5 => format!("{select} WHERE NOT (timestamp >= {a} AND timestamp <= {b})"),
            6 => format!("{select} WHERE timestamp = {a} OR timestamp = {b}"),
            7 => format!("{select} WHERE timestamp > {a}"),
            8 => format!("{select} WHERE {b} > timestamp"),
            _ => format!(
                "SELECT MAX(timestamp) AS timestamp, metric_name, labels, \
                 SUM(value) AS value FROM metrics WHERE timestamp >= {a} \
                 GROUP BY metric_name, labels"
            ),
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

        /// #377: whatever the files hold and whatever the query asks, the
        /// streamed, pruned scan returns exactly the rows the eager reader
        /// did. Pruning may only skip data no answer could contain. Files are
        /// small and timestamps dense, so bounds often land exactly on a
        /// file's first or last sample, where an off-by-one would show.
        #[test]
        fn streamed_reads_match_the_eager_reader(
            files in proptest::collection::vec(
                proptest::collection::vec((0usize..4, 0u64..16, 0usize..2), 1..5),
                0..8,
            ),
            buffer in proptest::collection::vec((0usize..4, 0u64..16, 0usize..2), 0..4),
            shape in 0usize..10,
            a in 0u64..18,
            b in 0u64..18,
            name in 0usize..4,
            other in 0usize..4,
        ) {
            let names = ["alpha", "beta", "delta", "gamma"];
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let (mut store, _dir) = test_store();
                let insert = |store: &mut MayoStore, rows: &[(usize, u64, usize)]| {
                    for (index, (series, timestamp, app)) in rows.iter().enumerate() {
                        let key = app_key(names[*series], &format!("default/app-{app}"), "i-0");
                        store.insert(&key, Sample::at(*timestamp, index as f64));
                    }
                };
                for file in &files {
                    insert(&mut store, file);
                    store.flush().await.unwrap();
                }
                insert(&mut store, &buffer);

                let sql = property_sql(shape, a, b, names[name], names[other]);
                let streamed = store.query_sql(&sql).await.unwrap();
                let eager = eager_rows(&store, &sql).await;
                assert_eq!(canonical(&streamed), canonical(&eager), "{sql}");
                if shape == 0 {
                    let floored = store.query_sql_since(&sql, a).await.unwrap();
                    assert_eq!(canonical(&floored), canonical(&eager), "{sql} since {a}");
                }
            });
        }
    }

    /// A flushed metrics file is the only copy once the buffer is cleared,
    /// so a local object store must sync it.
    #[test]
    fn local_metrics_object_store_syncs_its_writes() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().to_str().unwrap().to_string();
        let url = format!("file://{bare}");
        for destination in [bare, url] {
            let (store, _) = parse_object_store(&destination).unwrap();
            let store = format!("{store:?}");
            assert!(store.contains("fsync: true"), "{destination}: {store}");
        }
    }
    #[tokio::test]
    async fn simultaneous_object_store_writers_keep_both_nodes_history() {
        let bucket = tempfile::tempdir().unwrap();
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let url = url::Url::from_file_path(bucket.path()).unwrap().to_string();
        let mut first = MayoStore::open(first_dir.path().to_path_buf(), Some(&url))
            .await
            .unwrap();
        let mut second = MayoStore::open(second_dir.path().to_path_buf(), Some(&url))
            .await
            .unwrap();
        let first_key = MetricKey::with_labels(
            "cpu",
            std::collections::BTreeMap::from([("node".into(), "a".into())]),
        );
        let second_key = MetricKey::with_labels(
            "cpu",
            std::collections::BTreeMap::from([("node".into(), "b".into())]),
        );
        first.insert(&first_key, Sample::at(100, 11.0));
        second.insert(&second_key, Sample::at(100, 22.0));
        let (a, b) = tokio::join!(first.flush(), second.flush());
        a.unwrap();
        b.unwrap();
        let rows = first.query("cpu", 0, 200).await.unwrap();
        assert_eq!(
            rows.len(),
            2,
            "one node overwrote the other node's first chunk"
        );
        assert!(rows.iter().any(|row| row.3 == 11.0));
        assert!(rows.iter().any(|row| row.3 == 22.0));
        first.insert(&first_key, Sample::at(101, 12.0));
        second.insert(&second_key, Sample::at(101, 23.0));
        let (a, b) = tokio::join!(first.flush(), second.flush());
        a.unwrap();
        b.unwrap();
        let restarted = MayoStore::open(first_dir.path().to_path_buf(), Some(&url))
            .await
            .unwrap();
        assert_eq!(restarted.query("cpu", 0, 200).await.unwrap().len(), 4);
        assert_eq!(std::fs::read_dir(bucket.path()).unwrap().count(), 4);
    }
    #[tokio::test]
    async fn node_archives_are_scoped_but_generic_queries_read_all_owners() {
        let bucket = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let url = url::Url::from_file_path(bucket.path()).unwrap().to_string();
        let mut one =
            MayoStore::open_for_node(local.path().join("one"), Some(&url), "cluster:a/node:a")
                .await
                .unwrap();
        let mut two =
            MayoStore::open_for_node(local.path().join("two"), Some(&url), "cluster:a/node:b")
                .await
                .unwrap();
        let key = MetricKey::simple("cpu");
        one.insert(&key, Sample::at(100, 11.0));
        two.insert(&key, Sample::at(100, 22.0));
        one.flush().await.unwrap();
        two.flush().await.unwrap();
        assert_eq!(
            one.query("cpu", 0, 200)
                .await
                .unwrap()
                .iter()
                .map(|row| row.3)
                .collect::<Vec<_>>(),
            vec![11.0]
        );
        assert_eq!(
            two.query("cpu", 0, 200)
                .await
                .unwrap()
                .iter()
                .map(|row| row.3)
                .collect::<Vec<_>>(),
            vec![22.0]
        );
        let recovered = MayoStore::open_for_node(
            local.path().join("lost-local-dir"),
            Some(&url),
            "cluster:a/node:a",
        )
        .await
        .unwrap();
        assert_eq!(
            recovered
                .query("cpu", 0, 200)
                .await
                .unwrap()
                .iter()
                .map(|row| row.3)
                .collect::<Vec<_>>(),
            vec![11.0]
        );
        let all = MayoStore::open(local.path().join("archive-reader"), Some(&url))
            .await
            .unwrap();
        assert_eq!(all.query("cpu", 0, 200).await.unwrap().len(), 2);
        assert!(
            MayoStore::open_for_node(local.path().join("empty-owner"), Some(&url), "")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn create_only_remote_chunks_refuse_collisions_without_replacing_history() {
        let bucket = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let url = url::Url::from_file_path(bucket.path()).unwrap().to_string();
        let mut store = MayoStore::open(local.path().to_path_buf(), Some(&url))
            .await
            .unwrap();
        let key = MetricKey::simple("cpu");
        store.insert(&key, Sample::at(100, 11.0));
        let pending = store.take_flush_batch().unwrap().unwrap();
        let FlushTarget::Remote {
            store: remote,
            key: location,
        } = &pending.target
        else {
            panic!("expected remote flush")
        };
        let mut earlier = MayoStore::new(local.path().join("earlier"));
        earlier.insert(&key, Sample::at(100, 99.0));
        let bytes = batch_to_parquet_bytes(&earlier.buffer_to_batch().unwrap().unwrap()).unwrap();
        remote
            .put_opts(
                location,
                object_store::PutPayload::from(bytes),
                object_store::PutOptions {
                    mode: object_store::PutMode::Create,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(write_pending_flush(pending).await.is_err());
        assert_mayo_values(&store, &[11.0, 99.0]).await;
        // The store retains the batch and retargets only a proved collision.
        store.flush().await.unwrap();
        let mut values: Vec<_> = store
            .query("cpu", 0, 200)
            .await
            .unwrap()
            .iter()
            .map(|row| row.3)
            .collect();
        values.sort_by(f64::total_cmp);
        assert_eq!(values, vec![11.0, 99.0]);
    }
    // Insert inside src/mayo/store.rs::tests. Requires the separate cfg(test)
    // MayoWriteGate instrumentation patch; no production ownership fix here.

    #[derive(Debug)]
    struct GatedMayoObjectStore {
        inner: Arc<object_store::memory::InMemory>,
        gate: Arc<MayoWriteGate>,
        fail_after_commit: bool,
        fail_reads: std::sync::atomic::AtomicBool,
        attempts: std::sync::Mutex<Vec<object_store::path::Path>>,
    }

    impl std::fmt::Display for GatedMayoObjectStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "gated Mayo object store")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for GatedMayoObjectStore {
        async fn put_opts(
            &self,
            location: &object_store::path::Path,
            payload: object_store::PutPayload,
            options: object_store::PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            self.attempts.lock().unwrap().push(location.clone());
            assert!(matches!(options.mode, object_store::PutMode::Create));
            let _finished = MayoAttemptFinished(self.gate.clone());
            self.gate
                .wait_async(MayoWriteStage::BeforePublication)
                .await;
            let result = self.inner.put_opts(location, payload, options).await?;
            let first_committed = self.gate.wait_async(MayoWriteStage::AfterPublication).await;
            if first_committed && self.fail_after_commit {
                return Err(object_store::Error::Generic {
                    store: "gated Mayo fixture",
                    source: Box::new(std::io::Error::other("response lost after committed PUT")),
                });
            }
            Ok(result)
        }

        async fn put_multipart_opts(
            &self,
            location: &object_store::path::Path,
            options: object_store::PutMultipartOptions,
        ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
            self.inner.put_multipart_opts(location, options).await
        }

        async fn get_opts(
            &self,
            location: &object_store::path::Path,
            options: object_store::GetOptions,
        ) -> object_store::Result<object_store::GetResult> {
            if self.fail_reads.load(std::sync::atomic::Ordering::Acquire) {
                return Err(object_store::Error::Generic {
                    store: "gated Mayo fixture",
                    source: Box::new(std::io::Error::other(
                        "publication ownership read unavailable",
                    )),
                });
            }
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: futures_util::stream::BoxStream<
                'static,
                object_store::Result<object_store::path::Path>,
            >,
        ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>
        {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&object_store::path::Path>,
        ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
        {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&object_store::path::Path>,
        ) -> object_store::Result<object_store::ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &object_store::path::Path,
            to: &object_store::path::Path,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    fn gated_remote_mayo(
        directory: &std::path::Path,
        stage: MayoWriteStage,
        fail_after_commit: bool,
    ) -> (MayoStore, Arc<GatedMayoObjectStore>, Arc<MayoWriteGate>) {
        let gate = Arc::new(MayoWriteGate::new(stage));
        let remote = Arc::new(GatedMayoObjectStore {
            inner: Arc::new(object_store::memory::InMemory::new()),
            gate: gate.clone(),
            fail_after_commit,
            fail_reads: std::sync::atomic::AtomicBool::new(false),
            attempts: std::sync::Mutex::new(Vec::new()),
        });
        let mut store = MayoStore::new(directory.to_path_buf());
        store.backend = Backend::Remote {
            store: remote.clone(),
            prefix: object_store::path::Path::from("node-owned"),
        };
        (store, remote, gate)
    }

    async fn assert_mayo_values(store: &MayoStore, expected: &[f64]) {
        let mut actual: Vec<_> = store
            .query("cpu", 0, 200)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.3)
            .collect();
        actual.sort_by(f64::total_cmp);
        assert_eq!(
            actual, expected,
            "pending/publication query lost or duplicated a sample"
        );
    }

    async fn wait_for_mayo_gate(notification: &tokio::sync::Notify) {
        tokio::time::timeout(std::time::Duration::from_secs(5), notification.notified())
            .await
            .unwrap();
    }

    async fn assert_canceled_local_mayo_error_retains_owned_rows(shared: bool) {
        let directory = tempfile::tempdir().unwrap();
        let gate = Arc::new(MayoWriteGate::new(MayoWriteStage::BeforePublication));
        let _release_on_failure = ReleaseMayoWriteGate(gate.clone());
        let mut local = MayoStore::new(directory.path().to_path_buf());
        local.test_gate = Some(gate.clone());
        local.insert(&MetricKey::simple("cpu"), Sample::at(100, 11.0));
        // This makes the real atomic publication fail after the caller is canceled.
        // A directory cannot be replaced with the completed Parquet file.
        let refused_target = directory.path().join("metrics_000000.parquet");
        std::fs::create_dir(&refused_target).unwrap();
        let store = Arc::new(tokio::sync::RwLock::new(local));
        let writer = {
            let store = store.clone();
            tokio::spawn(async move {
                if shared {
                    flush_off_lock(&store).await.map(|_| ())
                } else {
                    store.write().await.flush().await
                }
            })
        };
        wait_for_mayo_gate(&gate.entered).await;
        writer.abort();
        assert!(writer.await.unwrap_err().is_cancelled());
        assert_eq!(
            store.read().await.buffer_len(),
            1,
            "canceling the caller discarded the only store-owned sample"
        );
        store
            .write()
            .await
            .insert(&MetricKey::simple("cpu"), Sample::at(101, 22.0));
        gate.release();
        wait_for_mayo_gate(&gate.finished).await;
        assert_eq!(
            store.read().await.buffer_len(),
            2,
            "a detached blocking writer failure discarded pending rows"
        );
        std::fs::remove_dir(&refused_target).unwrap();
        // The intentional directory blocker is not a valid archived Parquet file.
        // Remove it before querying, so the fixture tests retained ownership rather
        // than the scanner's treatment of a directory named *.parquet.
        assert_mayo_values(&*store.read().await, &[11.0, 22.0]).await;
        assert!(flush_off_lock(&store).await.unwrap());
        assert_mayo_values(&*store.read().await, &[11.0, 22.0]).await;
        assert_eq!(store.read().await.buffer_len(), 0);
        let reopened = MayoStore::new(directory.path().to_path_buf());
        assert_mayo_values(&reopened, &[11.0, 22.0]).await;
    }

    #[tokio::test]
    async fn canceled_local_mayo_error_retains_owned_rows_for_retry() {
        for shared in [false, true] {
            assert_canceled_local_mayo_error_retains_owned_rows(shared).await;
        }
    }

    async fn assert_canceled_local_mayo_publication_is_visible_once(shared: bool) {
        let directory = tempfile::tempdir().unwrap();
        let gate = Arc::new(MayoWriteGate::new(MayoWriteStage::AfterPublication));
        let _release_on_failure = ReleaseMayoWriteGate(gate.clone());
        let mut local = MayoStore::new(directory.path().to_path_buf());
        local.test_gate = Some(gate.clone());
        local.insert(&MetricKey::simple("cpu"), Sample::at(100, 11.0));
        let store = Arc::new(tokio::sync::RwLock::new(local));
        let writer = {
            let store = store.clone();
            tokio::spawn(async move {
                if shared {
                    flush_off_lock(&store).await.map(|_| ())
                } else {
                    store.write().await.flush().await
                }
            })
        };
        wait_for_mayo_gate(&gate.entered).await;
        assert!(directory.path().join("metrics_000000.parquet").is_file());
        writer.abort();
        assert!(writer.await.unwrap_err().is_cancelled());
        assert_eq!(
            store.read().await.buffer_len(),
            1,
            "published-but-unacknowledged rows must remain store-owned"
        );
        assert_mayo_values(&*store.read().await, &[11.0]).await;
        assert_eq!(
            store.read().await.prune(u64::MAX).unwrap(),
            0,
            "retention removed a still-owned publication before completion"
        );
        assert_mayo_values(&*store.read().await, &[11.0]).await;
        store
            .write()
            .await
            .insert(&MetricKey::simple("cpu"), Sample::at(101, 22.0));
        assert_mayo_values(&*store.read().await, &[11.0, 22.0]).await;
        gate.release();
        wait_for_mayo_gate(&gate.finished).await;
        assert!(flush_off_lock(&store).await.unwrap());
        assert_mayo_values(&*store.read().await, &[11.0, 22.0]).await;
        assert_mayo_values(
            &MayoStore::new(directory.path().to_path_buf()),
            &[11.0, 22.0],
        )
        .await;
    }

    #[tokio::test]
    async fn canceled_local_mayo_publication_is_visible_once_while_writer_is_owned() {
        for shared in [false, true] {
            assert_canceled_local_mayo_publication_is_visible_once(shared).await;
        }
    }

    async fn assert_canceled_remote_mayo_is_retryable(stage: MayoWriteStage, shared: bool) {
        use futures_util::TryStreamExt;
        let directory = tempfile::tempdir().unwrap();
        let (mut local, remote, gate) = gated_remote_mayo(directory.path(), stage, false);
        let _release_on_failure = ReleaseMayoWriteGate(gate.clone());
        local.insert(&MetricKey::simple("cpu"), Sample::at(100, 11.0));
        let store = Arc::new(tokio::sync::RwLock::new(local));
        let writer = {
            let store = store.clone();
            tokio::spawn(async move {
                if shared {
                    flush_off_lock(&store).await.map(|_| ())
                } else {
                    store.write().await.flush().await
                }
            })
        };
        wait_for_mayo_gate(&gate.entered).await;
        let before = remote
            .inner
            .list(None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(
            before.len(),
            usize::from(stage == MayoWriteStage::AfterPublication)
        );
        writer.abort();
        assert!(writer.await.unwrap_err().is_cancelled());
        wait_for_mayo_gate(&gate.finished).await;
        assert_eq!(
            store.read().await.buffer_len(),
            1,
            "canceling an uncertain remote PUT discarded its pending batch"
        );
        assert_mayo_values(&*store.read().await, &[11.0]).await;
        store
            .write()
            .await
            .insert(&MetricKey::simple("cpu"), Sample::at(101, 22.0));
        assert_mayo_values(&*store.read().await, &[11.0, 22.0]).await;
        assert!(flush_off_lock(&store).await.unwrap());
        assert_mayo_values(&*store.read().await, &[11.0, 22.0]).await;
        let attempts = remote.attempts.lock().unwrap().clone();
        assert_eq!(attempts.len(), 3, "one retry plus one new immutable chunk");
        assert_eq!(
            attempts[0], attempts[1],
            "uncertain PUT retried under a new key"
        );
        assert_ne!(attempts[1], attempts[2]);
        assert_eq!(
            remote
                .inner
                .list(None)
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
                .len(),
            2
        );
        let (mut reopened, _, _) = gated_remote_mayo(directory.path(), stage, false);
        reopened.backend = Backend::Remote {
            store: remote,
            prefix: object_store::path::Path::from("node-owned"),
        };
        assert_mayo_values(&reopened, &[11.0, 22.0]).await;
    }

    #[tokio::test]
    async fn canceled_remote_mayo_before_commit_keeps_rows_queryable() {
        for shared in [false, true] {
            assert_canceled_remote_mayo_is_retryable(MayoWriteStage::BeforePublication, shared)
                .await;
        }
    }

    #[tokio::test]
    async fn canceled_remote_mayo_after_commit_retries_the_same_verified_chunk() {
        for shared in [false, true] {
            assert_canceled_remote_mayo_is_retryable(MayoWriteStage::AfterPublication, shared)
                .await;
        }
    }

    #[tokio::test]
    async fn errored_remote_mayo_after_commit_neither_duplicates_nor_rekeys_its_rows() {
        use futures_util::TryStreamExt;
        let directory = tempfile::tempdir().unwrap();
        let (mut store, remote, gate) =
            gated_remote_mayo(directory.path(), MayoWriteStage::AfterPublication, true);
        let _release_on_failure = ReleaseMayoWriteGate(gate.clone());
        store.insert(&MetricKey::simple("cpu"), Sample::at(100, 11.0));
        let writer = tokio::spawn(async move {
            let result = store.flush().await;
            (store, result)
        });
        wait_for_mayo_gate(&gate.entered).await;
        gate.release();
        let (mut store, result) = writer.await.unwrap();
        assert!(
            result.is_err(),
            "fixture must lose the response after commit"
        );
        assert_mayo_values(&store, &[11.0]).await;
        store.flush().await.unwrap();
        assert_mayo_values(&store, &[11.0]).await;
        let attempts = remote.attempts.lock().unwrap().clone();
        assert_eq!(attempts.len(), 2);
        assert_eq!(
            attempts[0], attempts[1],
            "an uncertain failure allocated a fresh key"
        );
        assert_eq!(
            remote
                .inner
                .list(None)
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_foreign_identical_sample_chunk_is_kept_and_the_pending_publication_is_rekeyed() {
        use futures_util::TryStreamExt;
        let directory = tempfile::tempdir().unwrap();
        let remote = Arc::new(object_store::memory::InMemory::new());
        let mut store = MayoStore::new(directory.path().to_path_buf());
        store.backend = Backend::Remote {
            store: remote.clone(),
            prefix: object_store::path::Path::from("node-owned"),
        };
        store.insert(&MetricKey::simple("cpu"), Sample::at(100, 11.0));
        let pending = store.take_flush_batch().unwrap().unwrap();
        let FlushTarget::Remote { key, .. } = &pending.target else {
            panic!("expected remote target");
        };
        let refused_key = key.clone();
        // Independent publication identity, identical metric rows. A sample-only
        // comparison must never mistake this older publication for our pending one.
        let foreign_publication = format!("{:032x}", rand::random::<u128>());
        let properties = datafusion::parquet::file::properties::WriterProperties::builder()
            .set_key_value_metadata(Some(vec![datafusion::parquet::file::metadata::KeyValue {
                key: "reliaburger.mayo.publication".into(),
                value: Some(foreign_publication),
            }]))
            .build();
        let mut bytes = Vec::new();
        let mut writer =
            ArrowWriter::try_new(&mut bytes, pending.batch.schema(), Some(properties)).unwrap();
        writer.write(&pending.batch).unwrap();
        writer.close().unwrap();
        remote
            .put_opts(
                &refused_key,
                bytes.into(),
                object_store::PutOptions {
                    mode: object_store::PutMode::Create,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(write_pending_flush(pending).await.is_err());
        assert_mayo_values(&store, &[11.0, 11.0]).await;
        store.flush().await.unwrap();
        assert_mayo_values(&store, &[11.0, 11.0]).await;
        let objects = remote.list(None).try_collect::<Vec<_>>().await.unwrap();
        assert_eq!(
            objects.len(),
            2,
            "foreign history was replaced or pending rows lost"
        );
        assert!(objects.iter().any(|object| object.location == refused_key));
        let mut reopened = MayoStore::new(directory.path().to_path_buf());
        reopened.backend = Backend::Remote {
            store: remote,
            prefix: object_store::path::Path::from("node-owned"),
        };
        assert_mayo_values(&reopened, &[11.0, 11.0]).await;
    }

    #[test]
    fn failed_mayo_backlog_is_bounded_without_discarding_the_owned_publication() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = MayoStore::new(directory.path().to_path_buf());
        let key = MetricKey::simple("cpu");
        store.insert(&key, Sample::at(100, 11.0));
        let original = store.take_flush_batch().unwrap().unwrap();
        let original_path = match original.target {
            FlushTarget::Local { path, .. } => path,
            _ => unreachable!(),
        };
        for _ in 0..1_000_100 {
            store.insert(&key, Sample::at(101, 22.0));
        }
        assert!(
            store.buffer_len() <= 1_000_000,
            "failed publication grew the backlog beyond its row cap"
        );
        let retry = store.take_flush_batch().unwrap().unwrap();
        let retry_path = match retry.target {
            FlushTarget::Local { path, .. } => path,
            _ => unreachable!(),
        };
        assert_eq!(
            retry_path, original_path,
            "backlog shedding discarded the protected publication"
        );
        assert_eq!(retry.batch.num_rows(), 1);
        assert_eq!(
            retry
                .batch
                .column(3)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap()
                .value(0),
            11.0
        );
    }

    #[tokio::test]
    async fn unknown_remote_mayo_ownership_read_retains_the_same_publication() {
        use futures_util::TryStreamExt;
        let directory = tempfile::tempdir().unwrap();
        let (mut store, remote, gate) =
            gated_remote_mayo(directory.path(), MayoWriteStage::AfterPublication, true);
        let _release_on_failure = ReleaseMayoWriteGate(gate.clone());
        store.insert(&MetricKey::simple("cpu"), Sample::at(100, 11.0));
        let writer = tokio::spawn(async move {
            let result = store.flush().await;
            (store, result)
        });
        wait_for_mayo_gate(&gate.entered).await;
        gate.release();
        let (mut store, result) = writer.await.unwrap();
        assert!(
            result.is_err(),
            "fixture must lose the committed PUT response"
        );
        remote
            .fail_reads
            .store(true, std::sync::atomic::Ordering::Release);
        assert!(
            store.flush().await.is_err(),
            "unknown ownership must not allocate a fresh key"
        );
        assert_eq!(
            store.buffer_len(),
            1,
            "unproved publication lost its retained sample"
        );
        assert!(
            store.query("cpu", 0, 200).await.is_err(),
            "unknown ownership must not return duplicated or silently hidden rows"
        );
        remote
            .fail_reads
            .store(false, std::sync::atomic::Ordering::Release);
        store.flush().await.unwrap();
        assert_mayo_values(&store, &[11.0]).await;
        let attempts = remote.attempts.lock().unwrap().clone();
        assert_eq!(attempts.len(), 3);
        assert!(
            attempts.iter().all(|key| key == &attempts[0]),
            "transient ownership failure retargeted an uncertain publication"
        );
        assert_eq!(
            remote
                .inner
                .list(None)
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn local_mayo_parent_confirmation_failure_retries_the_owned_creation_chain() {
        use std::sync::atomic::Ordering;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("created-outer/created-inner/metrics");
        let gate = Arc::new(MayoWriteGate::new(MayoWriteStage::BeforePublication));
        let _release_on_failure = ReleaseMayoWriteGate(gate.clone());
        gate.fail_parent_sync.store(true, Ordering::Release);
        // Publication runs without waiting; the controlled fault applies only to
        // actual owned parent-entry confirmation, after real file publication.
        gate.release();
        let mut store = MayoStore::new(directory.clone());
        store.test_gate = Some(gate.clone());
        store.insert(&MetricKey::simple("cpu"), Sample::at(100, 11.0));
        let first = store.flush().await;
        assert!(
            matches!(first,
        Err(MayoError::Io(ref error))
        if error.to_string().contains("controlled Mayo parent-directory confirmation failure")),
            "flush must refuse uncertain owned-directory publication: {first:?}"
        );
        assert!(directory.join("metrics_000000.parquet").is_file());
        assert_eq!(
            store.buffer_len(),
            1,
            "uncertain directory entry lost retained ownership"
        );
        assert_mayo_values(&store, &[11.0]).await;
        let failed_visits = gate.parent_sync_visits.load(Ordering::Acquire);
        assert!(
            failed_visits > 0,
            "fixture never reached real parent confirmation"
        );
        gate.fail_parent_sync.store(false, Ordering::Release);
        store.flush().await.unwrap();
        // The newly created directories now exist. Retry must retain their original
        // three parent entries instead of rediscovering an empty creation chain.
        assert!(
            gate.parent_sync_visits.load(Ordering::Acquire) >= failed_visits + 3,
            "retry forgot owned ancestor entries after the first publication error"
        );
        assert_eq!(store.buffer_len(), 0);
        assert_mayo_values(&store, &[11.0]).await;
        let reopened = MayoStore::new(directory);
        assert_mayo_values(&reopened, &[11.0]).await;
    }

    #[tokio::test]
    async fn reopened_mayo_reconfirms_a_visible_owned_directory_entry_before_ack() {
        use std::sync::atomic::Ordering;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("owned-metrics");
        let first_gate = Arc::new(MayoWriteGate::new(MayoWriteStage::BeforePublication));
        let _first_release = ReleaseMayoWriteGate(first_gate.clone());
        first_gate.fail_parent_sync.store(true, Ordering::Release);
        first_gate.release();
        let mut first = MayoStore::new(directory.clone());
        first.test_gate = Some(first_gate);
        first.insert(&MetricKey::simple("cpu"), Sample::at(100, 11.0));
        let first_result = first.flush().await;
        assert!(
            matches!(first_result,
        Err(MayoError::Io(ref error))
        if error.to_string().contains("controlled Mayo parent-directory confirmation failure")),
            "first publication did not reach its controlled uncertainty: {first_result:?}"
        );
        assert!(directory.join("metrics_000000.parquet").is_file());
        drop(first);
        // Reopen a genuine visible archive left by the failed writer. The fresh
        // handle cannot use the old in-memory missing-directory inventory.
        let gate = Arc::new(MayoWriteGate::new(MayoWriteStage::BeforePublication));
        let _release_on_failure = ReleaseMayoWriteGate(gate.clone());
        gate.fail_parent_sync.store(true, Ordering::Release);
        gate.release();
        let mut reopened = MayoStore::new(directory.clone());
        reopened.test_gate = Some(gate.clone());
        reopened.insert(&MetricKey::simple("cpu"), Sample::at(101, 22.0));
        let result = reopened.flush().await;
        assert!(
            matches!(result,
        Err(MayoError::Io(ref error))
        if error.to_string().contains("controlled Mayo parent-directory confirmation failure")),
            "fresh store acknowledged an unconfirmed visible owned directory: {result:?}"
        );
        assert_eq!(reopened.buffer_len(), 1);
        assert_mayo_values(&reopened, &[11.0, 22.0]).await;
        let failed_visits = gate.parent_sync_visits.load(Ordering::Acquire);
        assert!(
            failed_visits > 0,
            "fresh store skipped configured directory-entry confirmation"
        );
        gate.fail_parent_sync.store(false, Ordering::Release);
        reopened.flush().await.unwrap();
        assert!(gate.parent_sync_visits.load(Ordering::Acquire) > failed_visits);
        assert_eq!(reopened.buffer_len(), 0);
        assert_mayo_values(&MayoStore::new(directory), &[11.0, 22.0]).await;
    }

    fn mayo_creation_confirmation_fixture(
        directory: &std::path::Path,
        child: &std::path::Path,
    ) -> (MayoStore, Arc<MayoCreationConfirmationFault>) {
        let fault = Arc::new(MayoCreationConfirmationFault {
            child: child.to_path_buf(),
            failing: std::sync::atomic::AtomicBool::new(true),
            visits: std::sync::Mutex::new(Vec::new()),
        });
        let gate = Arc::new(MayoWriteGate::new(MayoWriteStage::BeforePublication));
        gate.release();
        *gate.creation_confirmation_fault.lock().unwrap() = Some(fault.clone());
        let mut store = MayoStore::new(directory.to_path_buf());
        store.test_gate = Some(gate);
        (store, fault)
    }

    #[tokio::test]
    async fn missing_mayo_ancestor_is_confirmed_before_descendants_or_publication() {
        use std::sync::atomic::Ordering;
        let root = tempfile::tempdir().unwrap();
        let outer = root.path().join("owned-outer");
        let inner = outer.join("owned-inner");
        let directory = inner.join("metrics");
        let (mut store, fault) = mayo_creation_confirmation_fixture(&directory, &outer);
        store.insert(&MetricKey::simple("cpu"), Sample::at(100, 11.0));
        let result = store.flush().await;
        assert!(
            matches!(result, Err(MayoError::Io(ref error))
            if error.to_string().contains("controlled Mayo creation-entry confirmation failure")),
            "missing ancestor confirmation was not refused: {result:?}"
        );
        assert_eq!(&*fault.visits.lock().unwrap(), std::slice::from_ref(&outer));
        assert!(outer.is_dir(), "fault must follow its actual mkdir");
        assert!(
            !inner.exists(),
            "descendant was created before ancestor confirmation"
        );
        assert!(
            !directory.exists(),
            "publication tree survived an unconfirmed ancestor"
        );
        assert_eq!(store.buffer_len(), 1);
        assert_mayo_values(&store, &[11.0]).await;
        store.insert(&MetricKey::simple("cpu"), Sample::at(101, 22.0));
        assert_mayo_values(&store, &[11.0, 22.0]).await;
        fault.failing.store(false, Ordering::Release);
        store.flush().await.unwrap();
        assert!(
            fault.visits.lock().unwrap().len() >= 2,
            "retry failed to reconfirm the visible outer entry"
        );
        assert_eq!(store.buffer_len(), 0);
        assert_mayo_values(&store, &[11.0, 22.0]).await;
        let parquet = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "parquet")
            })
            .count();
        assert_eq!(parquet, 2, "each immutable pending batch must publish once");
        assert_mayo_values(&MayoStore::new(directory), &[11.0, 22.0]).await;
    }

    #[tokio::test]
    async fn fresh_mayo_store_reconfirms_visible_outer_entry_before_descent() {
        use std::sync::atomic::Ordering;
        let root = tempfile::tempdir().unwrap();
        let outer = root.path().join("owned-outer");
        let inner = outer.join("owned-inner");
        let directory = inner.join("metrics");
        // First configure the outer directory itself. This leaves just that
        // shared entry uncertain even under the current post-publication
        // proposal, so the second half independently tests a fresh handle.
        let (mut first, first_fault) = mayo_creation_confirmation_fixture(&outer, &outer);
        first.insert(&MetricKey::simple("cpu"), Sample::at(100, 11.0));
        let result = first.flush().await;
        assert!(
            matches!(result, Err(MayoError::Io(ref error))
            if error.to_string().contains("controlled Mayo creation-entry confirmation failure")),
            "first writer never reached entry-confirmation uncertainty: {result:?}"
        );
        assert_eq!(
            &*first_fault.visits.lock().unwrap(),
            std::slice::from_ref(&outer)
        );
        assert!(outer.is_dir());
        assert!(!inner.exists());
        assert!(!directory.exists());
        assert_eq!(first.buffer_len(), 1);
        assert_mayo_values(&first, &[11.0]).await;
        drop(first);
        // The fixed writer has not published its old buffer. The intermediate
        // proposal may have published in `outer`, outside the fresh deeper
        // archive. This case does not claim recovery of uncommitted memory.
        let (mut fresh, fault) = mayo_creation_confirmation_fixture(&directory, &outer);
        fresh.insert(&MetricKey::simple("cpu"), Sample::at(101, 22.0));
        let result = fresh.flush().await;
        assert!(
            matches!(result, Err(MayoError::Io(ref error))
            if error.to_string().contains("controlled Mayo creation-entry confirmation failure")),
            "fresh handle descended through an unconfirmed visible outer entry: {result:?}"
        );
        assert_eq!(&*fault.visits.lock().unwrap(), std::slice::from_ref(&outer));
        assert!(!inner.exists());
        assert!(!directory.exists());
        assert_eq!(fresh.buffer_len(), 1);
        assert_mayo_values(&fresh, &[22.0]).await;
        fault.failing.store(false, Ordering::Release);
        fresh.flush().await.unwrap();
        assert!(fault.visits.lock().unwrap().len() >= 2);
        assert_eq!(fresh.buffer_len(), 0);
        assert_mayo_values(&fresh, &[22.0]).await;
        let parquet = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "parquet")
            })
            .count();
        assert_eq!(parquet, 1);
        assert_mayo_values(&MayoStore::new(directory), &[22.0]).await;
    }
}
