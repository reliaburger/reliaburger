//! The durable, bounded audit log each node keeps (#685).
//!
//! Audit records used to live only in the 1024-entry event ring, beside
//! health, deploy and job events. A busy task array pushed them out within
//! seconds and a restart emptied the ring, so after an incident nobody could
//! answer "who did what". This module gives them a store of their own: an
//! append-only log of JSON Lines under `[storage] data/audit/`, one record
//! per action, synced to disk before the API answers the caller.
//!
//! The log is split into segments and bounded in total
//! (`[storage] audit_max_bytes`, 64 MiB by default). When a new record would
//! take the newest segment past its share of the bound, a new segment starts;
//! when the segments together pass the bound, the oldest one is deleted. So
//! the log keeps the most recent history and never grows without limit.
//!
//! Writing is done by one dedicated thread fed through a channel: file I/O
//! blocks, and blocking a Tokio worker would stall every other task on it. A
//! write that fails is logged and counted, but it never fails the action
//! being audited: an operator whose disk is full must still be able to stop
//! the app that is filling it.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use super::events::{AuditEvent, EventKind, EventSeverity};

pub mod refusals;

/// Default bound on the whole audit log of one node: 64 MiB.
pub const DEFAULT_AUDIT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// The smallest bound `node.toml` may set: 64 KiB.
pub const MIN_AUDIT_MAX_BYTES: u64 = 64 * 1024;

/// How many segments the bound is shared between.
const SEGMENTS_PER_LOG: u64 = 8;

/// How long an API call waits for its record to reach the disk before it
/// answers anyway.
pub const AUDIT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Records waiting for the writer thread. Audit traffic is a few records a
/// second at most; a queue this deep only fills when the disk has stopped.
const QUEUE_CAPACITY: usize = 4096;

/// Records `GET /v1/audit` returns when the caller names no limit.
pub const DEFAULT_QUERY_LIMIT: usize = 100;

/// The most records one `GET /v1/audit` returns.
pub const MAX_QUERY_LIMIT: usize = 10_000;

/// The subdirectory of `[storage] data` that holds the log.
pub const AUDIT_DIRECTORY: &str = "audit";

/// Segment files are `audit-<first sequence>.jsonl`, zero-padded so that
/// sorting the names sorts the segments oldest first.
const SEGMENT_PREFIX: &str = "audit-";
const SEGMENT_SUFFIX: &str = ".jsonl";

/// One audited action as the log stores it: a line of JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditRecord {
    /// Position in this node's log. It keeps counting across restarts, so
    /// together with `node` it names a record uniquely.
    pub sequence: u64,
    /// Unix timestamp in seconds.
    pub timestamp: u64,
    /// Event category.
    pub kind: EventKind,
    /// Event importance.
    pub severity: EventSeverity,
    /// Stable machine-readable action, for example `app.deleted`.
    pub action: String,
    /// The authenticated principal that asked for the action.
    pub principal: String,
    /// Related app, when the action has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Related namespace, when the action has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// The node that recorded the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// The address the request came from, as this node saw it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_address: Option<String>,
    /// Stable action-specific facts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, String>,
    /// Human-readable description.
    pub message: String,
}

/// What `GET /v1/audit` and `relish audit` return.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditListing {
    /// Matching records, oldest first.
    pub records: Vec<AuditRecord>,
    /// One message per node that couldn't be read, or whose log has lost
    /// writes since it started.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Errors opening or reading the audit log.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// The log directory couldn't be created or listed.
    #[error("audit log directory {path}: {source}")]
    Directory {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A segment couldn't be read or repaired.
    #[error("audit log segment {path}: {source}")]
    Segment {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The writer thread couldn't be started.
    #[error("audit log writer thread: {0}")]
    Thread(std::io::Error),
    /// The blocking read task panicked or was cancelled.
    #[error("audit log read was interrupted: {0}")]
    Interrupted(String),
}

/// How large the log may grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditLimits {
    /// The bound on every segment together.
    pub max_total_bytes: u64,
    /// The size at which a new segment starts.
    pub segment_bytes: u64,
}

impl AuditLimits {
    /// Limits for a total bound, shared evenly between the segments.
    pub fn from_total(max_total_bytes: u64) -> Self {
        Self {
            max_total_bytes,
            segment_bytes: (max_total_bytes / SEGMENTS_PER_LOG).max(1),
        }
    }
}

/// Filters for reading the log. Every filter that is set must match.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditFilter {
    /// Records at or after this Unix time, in seconds.
    #[serde(default)]
    pub since: Option<u64>,
    /// Records at or before this Unix time, in seconds.
    #[serde(default)]
    pub until: Option<u64>,
    /// An exact action, or a family ending in `*` (`fault.*`).
    #[serde(default)]
    pub action: Option<String>,
    /// A principal id, or the token name it was recorded under.
    #[serde(default)]
    pub principal: Option<String>,
    /// The namespace the action touched.
    #[serde(default)]
    pub namespace: Option<String>,
    /// How many of the newest matching records to return.
    #[serde(default)]
    pub limit: Option<usize>,
}

impl AuditFilter {
    /// The limit to apply: the caller's, clamped, or the default.
    pub fn effective_limit(&self) -> usize {
        self.limit
            .unwrap_or(DEFAULT_QUERY_LIMIT)
            .clamp(1, MAX_QUERY_LIMIT)
    }

    /// Whether `record` passes every filter that is set.
    pub fn matches(&self, record: &AuditRecord) -> bool {
        self.since.is_none_or(|since| record.timestamp >= since)
            && self.until.is_none_or(|until| record.timestamp <= until)
            && self
                .action
                .as_deref()
                .is_none_or(|action| match action.strip_suffix('*') {
                    Some(prefix) => record.action.starts_with(prefix),
                    None => record.action == action,
                })
            && self.principal.as_deref().is_none_or(|principal| {
                record.principal == principal
                    || record.details.get("token_name").map(String::as_str) == Some(principal)
            })
            && self
                .namespace
                .as_deref()
                .is_none_or(|namespace| record.namespace.as_deref() == Some(namespace))
    }
}

/// Keep the newest `limit` records of `records`, oldest first, ordered by
/// time, then node, then sequence. Used to merge the answers of every node.
pub fn newest(mut records: Vec<AuditRecord>, limit: usize) -> Vec<AuditRecord> {
    records.sort_by(|a, b| {
        (a.timestamp, &a.node, a.sequence).cmp(&(b.timestamp, &b.node, b.sequence))
    });
    let excess = records.len().saturating_sub(limit);
    records.drain(..excess);
    records
}

/// Replace anything shaped like a Reliaburger secret (API tokens and join
/// tokens both start `rbrg_`) with a marker. Handlers never put secrets in a
/// record; this is the second line of defence should one ever slip through.
pub fn redact_secrets(text: &str) -> String {
    const MARKER: &str = "rbrg_";
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(MARKER) {
        redacted.push_str(&rest[..start]);
        let after = &rest[start + MARKER.len()..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        redacted.push_str("rbrg_[redacted]");
        rest = &after[end..];
    }
    redacted.push_str(rest);
    redacted
}

/// Facts a handler adds to the record its route's audit makes: the
/// namespace a manifest named, the node a decommission targets, the id of
/// the lease it created. Path parameters are picked up without one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditNote {
    /// The app the action touched, when the path doesn't name it.
    pub app: Option<String>,
    /// The namespace the action touched, when the path doesn't name it.
    pub namespace: Option<String>,
    /// Further facts. Never a secret.
    pub details: BTreeMap<String, String>,
}

/// What the API knows about the request being handled, for audit.
struct RequestAudit {
    caller: Option<SocketAddr>,
    note: std::cell::RefCell<AuditNote>,
}

tokio::task_local! {
    /// The request the current task is handling. The API's audit layer sets
    /// it around each request, so code deep inside a handler can name the
    /// caller's address, or add to the request's record, without threading
    /// either through every call.
    static REQUEST: RequestAudit;
}

/// Run `future`, the handling of one request from `caller`, and return its
/// output with whatever the handler added through [`annotate`].
pub async fn scope_request<F: std::future::Future>(
    caller: Option<SocketAddr>,
    future: F,
) -> (F::Output, AuditNote) {
    let request = RequestAudit {
        caller,
        note: std::cell::RefCell::new(AuditNote::default()),
    };
    REQUEST
        .scope(request, async move {
            let output = future.await;
            let note = REQUEST
                .try_with(|request| request.note.take())
                .unwrap_or_default();
            (output, note)
        })
        .await
}

/// Name the workload the request being handled acts on, for its audit
/// record.
pub fn annotate_workload(app: &str, namespace: &str) {
    annotate(AuditNote {
        app: Some(app.to_string()),
        namespace: Some(namespace.to_string()),
        details: BTreeMap::new(),
    });
}

/// The address of the client whose request is being handled, when known.
pub fn caller_address() -> Option<SocketAddr> {
    REQUEST.try_with(|request| request.caller).ok().flatten()
}

/// Add `note` to the audit record of the request being handled. Fields it
/// sets replace earlier ones; details are merged. Outside a request (a
/// background task) it does nothing.
pub fn annotate(note: AuditNote) {
    let _ = REQUEST.try_with(|request| {
        let mut current = request.note.borrow_mut();
        if note.app.is_some() {
            current.app = note.app;
        }
        if note.namespace.is_some() {
            current.namespace = note.namespace;
        }
        current.details.extend(note.details);
    });
}

/// One record on its way to the writer thread.
struct WriteRequest {
    event: AuditEvent,
    written: Option<oneshot::Sender<bool>>,
}

/// A handle to one node's audit log. Cloning it is cheap; every clone feeds
/// the same writer thread.
#[derive(Clone)]
pub struct AuditLog {
    directory: Arc<PathBuf>,
    queue: mpsc::Sender<WriteRequest>,
    failures: Arc<AtomicU64>,
}

impl std::fmt::Debug for AuditLog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuditLog")
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

/// The answer to [`AuditLog::submit`]: wait on it to learn whether the
/// record reached the disk, or drop it not to.
pub struct AuditReceipt(Option<oneshot::Receiver<bool>>);

impl AuditReceipt {
    /// A receipt for a record that was never queued.
    pub fn none() -> Self {
        Self(None)
    }

    /// Wait until the record is synced to disk, at most
    /// [`AUDIT_WRITE_TIMEOUT`]. `false` when it wasn't queued, the write
    /// failed, or it took too long; the caller carries on either way.
    pub async fn written(self) -> bool {
        let Some(receiver) = self.0 else {
            return false;
        };
        matches!(
            tokio::time::timeout(AUDIT_WRITE_TIMEOUT, receiver).await,
            Ok(Ok(true))
        )
    }
}

impl AuditLog {
    /// Open (or create) the log in `directory` and start its writer thread.
    ///
    /// The thread runs until every handle is dropped. Opening repairs a
    /// segment a crash left with a half-written last line, and trims the log
    /// to `limits` in case the bound was lowered since the last run.
    pub fn open(directory: PathBuf, limits: AuditLimits) -> Result<Self, AuditError> {
        let writer = SegmentWriter::open(directory.clone(), limits)?;
        let failures = Arc::new(AtomicU64::new(0));
        let (queue, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let thread_failures = Arc::clone(&failures);
        std::thread::Builder::new()
            .name("audit-writer".to_string())
            .spawn(move || run_writer(writer, receiver, thread_failures))
            .map_err(AuditError::Thread)?;
        Ok(Self {
            directory: Arc::new(directory),
            queue,
            failures,
        })
    }

    /// The directory the log lives in.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Queue `event` for writing without waiting. The receipt says when (and
    /// whether) it reached the disk. A full queue counts as a failed write.
    pub fn submit(&self, event: AuditEvent) -> AuditReceipt {
        let (written, receiver) = oneshot::channel();
        match self.queue.try_send(WriteRequest {
            event,
            written: Some(written),
        }) {
            Ok(()) => AuditReceipt(Some(receiver)),
            Err(error) => {
                self.failures.fetch_add(1, Ordering::Relaxed);
                eprintln!("bun: audit record dropped: {error}");
                AuditReceipt::none()
            }
        }
    }

    /// How many records have failed to reach the disk since the log opened.
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// Read the newest records matching `filter`, oldest first.
    ///
    /// The read runs on Tokio's blocking pool. It may run while the writer
    /// appends or drops a segment, so a half-written line is skipped and a
    /// segment deleted mid-read is treated as empty.
    pub async fn query(&self, filter: AuditFilter) -> Result<Vec<AuditRecord>, AuditError> {
        let directory = Arc::clone(&self.directory);
        tokio::task::spawn_blocking(move || read_records(&directory, &filter))
            .await
            .map_err(|error| AuditError::Interrupted(error.to_string()))?
    }
}

/// The writer thread: take each record off the queue, write and sync it,
/// and report back.
fn run_writer(
    mut writer: SegmentWriter,
    mut receiver: mpsc::Receiver<WriteRequest>,
    failures: Arc<AtomicU64>,
) {
    while let Some(request) = receiver.blocking_recv() {
        let written = match writer.append(request.event) {
            Ok(()) => true,
            Err(error) => {
                failures.fetch_add(1, Ordering::Relaxed);
                eprintln!("bun: audit record not written: {error}");
                false
            }
        };
        if let Some(reply) = request.written {
            let _ = reply.send(written);
        }
    }
}

/// The writer thread's view of the log: the segments on disk, the open
/// newest one, and the next sequence number.
struct SegmentWriter {
    directory: PathBuf,
    limits: AuditLimits,
    /// Oldest first, with their sizes in bytes.
    segments: VecDeque<(PathBuf, u64)>,
    current: Option<std::fs::File>,
    next_sequence: u64,
}

impl SegmentWriter {
    fn open(directory: PathBuf, limits: AuditLimits) -> Result<Self, AuditError> {
        std::fs::create_dir_all(&directory).map_err(|source| AuditError::Directory {
            path: directory.clone(),
            source,
        })?;
        let segments = list_segments(&directory)?;
        let mut writer = Self {
            directory,
            limits,
            segments,
            current: None,
            next_sequence: 1,
        };
        writer.next_sequence = writer.last_sequence()?.saturating_add(1);
        writer.repair_newest()?;
        writer.prune();
        Ok(writer)
    }

    /// The highest sequence on disk, or 0 for an empty log.
    fn last_sequence(&self) -> Result<u64, AuditError> {
        for (path, _) in self.segments.iter().rev() {
            let records = read_segment(path)?;
            if let Some(last) = records.last() {
                return Ok(last.sequence);
            }
        }
        Ok(0)
    }

    /// End a half-written last line with a newline, so the next record
    /// starts on a line of its own (the broken line is skipped on read).
    fn repair_newest(&mut self) -> Result<(), AuditError> {
        let Some((path, size)) = self.segments.back_mut() else {
            return Ok(());
        };
        let contents = std::fs::read(&*path).map_err(|source| AuditError::Segment {
            path: path.clone(),
            source,
        })?;
        if contents.last().is_some_and(|byte| *byte != b'\n') {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&*path)
                .and_then(|mut file| file.write_all(b"\n").map(|()| file))
                .map_err(|source| AuditError::Segment {
                    path: path.clone(),
                    source,
                })?;
            let _ = file.flush();
            *size += 1;
        }
        Ok(())
    }

    /// Write one record, synced, rolling to a new segment and dropping the
    /// oldest ones as the limits require.
    fn append(&mut self, event: AuditEvent) -> std::io::Result<()> {
        let record = to_record(self.next_sequence, event);
        let mut line = serde_json::to_vec(&record).map_err(std::io::Error::other)?;
        line.push(b'\n');
        let length = line.len() as u64;
        let roll = match self.segments.back() {
            None => true,
            Some((_, size)) => *size > 0 && size + length > self.limits.segment_bytes,
        };
        if roll || self.current.is_none() {
            self.open_segment(roll)?;
        }
        let Some(file) = self.current.as_mut() else {
            return Err(std::io::Error::other("no open audit segment"));
        };
        file.write_all(&line)?;
        file.sync_data()?;
        if let Some((_, size)) = self.segments.back_mut() {
            *size += length;
        }
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.prune();
        Ok(())
    }

    /// Open the segment to append to: a fresh one named after the next
    /// sequence when `fresh`, otherwise the newest on disk.
    fn open_segment(&mut self, fresh: bool) -> std::io::Result<()> {
        self.current = None;
        if fresh {
            let path = self.directory.join(format!(
                "{SEGMENT_PREFIX}{:020}{SEGMENT_SUFFIX}",
                self.next_sequence
            ));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            // Make the new name itself durable, not just the bytes in it.
            if let Ok(directory) = std::fs::File::open(&self.directory) {
                let _ = directory.sync_all();
            }
            self.segments.push_back((path, 0));
            self.current = Some(file);
        } else if let Some((path, _)) = self.segments.back() {
            self.current = Some(std::fs::OpenOptions::new().append(true).open(path)?);
        }
        Ok(())
    }

    /// Delete the oldest segments until the log fits its bound. The newest
    /// segment always stays.
    fn prune(&mut self) {
        let mut total: u64 = self.segments.iter().map(|(_, size)| size).sum();
        while total > self.limits.max_total_bytes && self.segments.len() > 1 {
            let Some((path, size)) = self.segments.pop_front() else {
                break;
            };
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    eprintln!(
                        "bun: could not drop old audit segment {}: {error}",
                        path.display()
                    );
                }
            }
            total = total.saturating_sub(size);
        }
    }
}

/// Turn an audit event into the record the log stores, scrubbing anything
/// shaped like a secret.
fn to_record(sequence: u64, event: AuditEvent) -> AuditRecord {
    AuditRecord {
        sequence,
        timestamp: event.timestamp,
        kind: event.kind,
        severity: event.severity,
        action: event.action,
        principal: event.principal,
        app: event.app,
        namespace: event.namespace,
        node: event.node,
        source_address: event.source_address.map(|address| address.to_string()),
        details: event
            .details
            .into_iter()
            .map(|(key, value)| (key, redact_secrets(&value)))
            .collect(),
        message: redact_secrets(&event.message),
    }
}

/// The segment files in `directory`, oldest first, with their sizes.
fn list_segments(directory: &Path) -> Result<VecDeque<(PathBuf, u64)>, AuditError> {
    let entries = std::fs::read_dir(directory).map_err(|source| AuditError::Directory {
        path: directory.to_path_buf(),
        source,
    })?;
    let mut segments: Vec<(PathBuf, u64)> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.starts_with(SEGMENT_PREFIX) && name.ends_with(SEGMENT_SUFFIX)
            })
        })
        .map(|entry| {
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            (entry.path(), size)
        })
        .collect();
    segments.sort();
    Ok(segments.into())
}

/// Every whole record in one segment. A segment that has just been dropped
/// reads as empty; a line that doesn't parse (a crash mid-write) is skipped.
fn read_segment(path: &Path) -> Result<Vec<AuditRecord>, AuditError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(AuditError::Segment {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let mut records = Vec::new();
    for line in std::io::BufReader::new(file).split(b'\n') {
        let line = line.map_err(|source| AuditError::Segment {
            path: path.to_path_buf(),
            source,
        })?;
        if let Ok(record) = serde_json::from_slice::<AuditRecord>(&line) {
            records.push(record);
        }
    }
    Ok(records)
}

/// The newest `filter.limit` matching records in `directory`, oldest first.
fn read_records(directory: &Path, filter: &AuditFilter) -> Result<Vec<AuditRecord>, AuditError> {
    let limit = filter.effective_limit();
    let mut newest_first: Vec<AuditRecord> = Vec::new();
    // Walk the segments newest first and stop once the limit is met, so a
    // query for the latest records doesn't read the whole log.
    for (path, _) in list_segments(directory)?.iter().rev() {
        let mut records = read_segment(path)?;
        records.retain(|record| filter.matches(record));
        newest_first.extend(records.into_iter().rev());
        if newest_first.len() >= limit {
            break;
        }
    }
    newest_first.truncate(limit);
    newest_first.reverse();
    Ok(newest_first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(action: &str, namespace: Option<&str>) -> AuditEvent {
        AuditEvent {
            timestamp: 1_700_000_000,
            kind: EventKind::Operation,
            severity: EventSeverity::Info,
            action: action.to_string(),
            principal: "token:alice".to_string(),
            app: Some("web".to_string()),
            namespace: namespace.map(str::to_string),
            node: Some("node-1".to_string()),
            source_address: Some("192.0.2.7:51234".parse().unwrap()),
            details: BTreeMap::from([("token_name".to_string(), "alice".to_string())]),
            message: format!("{action} by alice"),
        }
    }

    fn total_bytes(directory: &Path) -> u64 {
        list_segments(directory)
            .unwrap()
            .iter()
            .map(|(_, size)| size)
            .sum()
    }

    fn everything() -> AuditFilter {
        AuditFilter {
            limit: Some(MAX_QUERY_LIMIT),
            ..AuditFilter::default()
        }
    }

    #[tokio::test]
    async fn audit_record_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit");
        {
            let log = AuditLog::open(path.clone(), AuditLimits::from_total(1 << 20)).unwrap();
            assert!(log.submit(event("token.created", None)).written().await);
        }
        // A fresh handle, as after a Bun restart, reads the record back and
        // carries on numbering after it.
        let log = AuditLog::open(path, AuditLimits::from_total(1 << 20)).unwrap();
        assert!(
            log.submit(event("app.deleted", Some("team-a")))
                .written()
                .await
        );
        let records = log.query(everything()).await.unwrap();
        let actions: Vec<_> = records.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(actions, ["token.created", "app.deleted"]);
        assert_eq!(records[0].sequence, 1);
        assert_eq!(records[1].sequence, 2);
        assert_eq!(
            records[0].source_address.as_deref(),
            Some("192.0.2.7:51234")
        );
    }

    #[tokio::test]
    async fn audit_log_rotates_and_stays_within_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit");
        let limits = AuditLimits::from_total(8 * 1024);
        let log = AuditLog::open(path.clone(), limits).unwrap();
        for _ in 0..400 {
            assert!(
                log.submit(event("app.applied", Some("default")))
                    .written()
                    .await
            );
        }
        assert!(total_bytes(&path) <= limits.max_total_bytes);
        let segments = list_segments(&path).unwrap();
        assert!(segments.len() > 1, "the log never rotated");
        for (_, size) in &segments {
            assert!(*size <= limits.segment_bytes);
        }
        // The newest record is always kept.
        let newest = log.query(AuditFilter::default()).await.unwrap();
        assert_eq!(newest.last().map(|r| r.sequence), Some(400));
    }

    #[tokio::test]
    async fn oldest_segment_is_dropped_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit");
        let log = AuditLog::open(path.clone(), AuditLimits::from_total(8 * 1024)).unwrap();
        for _ in 0..400 {
            assert!(log.submit(event("app.applied", None)).written().await);
        }
        let records = log.query(everything()).await.unwrap();
        let first = records.first().map(|r| r.sequence).unwrap();
        assert!(first > 1, "the oldest records were kept");
        // What survives is one unbroken run ending at the newest record:
        // only whole segments from the old end were dropped.
        let sequences: Vec<u64> = records.iter().map(|r| r.sequence).collect();
        let expected: Vec<u64> = (first..=400).collect();
        assert_eq!(sequences, expected);
    }

    #[tokio::test]
    async fn a_failed_audit_write_does_not_fail_the_action() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit");
        // One record per segment, so the next record needs a new file.
        let limits = AuditLimits {
            max_total_bytes: 1 << 20,
            segment_bytes: 1,
        };
        let log = AuditLog::open(path.clone(), limits).unwrap();
        assert!(log.submit(event("app.applied", None)).written().await);
        // Put a plain file where the directory was: creating the next
        // segment fails even for root.
        std::fs::remove_dir_all(&path).unwrap();
        std::fs::write(&path, b"not a directory").unwrap();
        let written = log.submit(event("app.deleted", None)).written().await;
        assert!(!written, "a write into a missing directory succeeded");
        assert_eq!(log.failures(), 1);
        // The handle keeps working: the caller's action is never blocked or
        // failed by the log, and later writes are attempted again.
        assert!(!log.submit(event("app.stopped", None)).written().await);
        assert_eq!(log.failures(), 2);
    }

    #[tokio::test]
    async fn records_never_contain_a_token_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit");
        let log = AuditLog::open(path.clone(), AuditLimits::from_total(1 << 20)).unwrap();
        let secret = format!("rbrg_{}", "ab".repeat(32));
        let join = format!("rbrg_join_1_{}", "cd".repeat(32));
        let mut leaky = event("token.created", None);
        leaky.message = format!("created {secret}");
        leaky.details.insert("join".to_string(), join.clone());
        assert!(log.submit(leaky).written().await);
        let on_disk: String = list_segments(&path)
            .unwrap()
            .iter()
            .map(|(segment, _)| std::fs::read_to_string(segment).unwrap())
            .collect();
        assert!(!on_disk.contains(&secret));
        assert!(!on_disk.contains(&join));
        assert!(on_disk.contains("rbrg_[redacted]"));
    }

    #[tokio::test]
    async fn a_half_written_line_is_skipped_and_writing_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit");
        {
            let log = AuditLog::open(path.clone(), AuditLimits::from_total(1 << 20)).unwrap();
            assert!(log.submit(event("app.applied", None)).written().await);
        }
        let (segment, _) = list_segments(&path).unwrap().pop_back().unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&segment)
            .unwrap();
        file.write_all(b"{\"sequence\":2,\"timest").unwrap();
        let log = AuditLog::open(path, AuditLimits::from_total(1 << 20)).unwrap();
        assert!(log.submit(event("app.deleted", None)).written().await);
        let actions: Vec<String> = log
            .query(everything())
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.action)
            .collect();
        assert_eq!(actions, ["app.applied", "app.deleted"]);
    }

    #[tokio::test]
    async fn query_filters_by_action_principal_namespace_and_time() {
        let dir = tempfile::tempdir().unwrap();
        let log =
            AuditLog::open(dir.path().join("audit"), AuditLimits::from_total(1 << 20)).unwrap();
        let mut early = event("fault.injected", Some("team-a"));
        early.timestamp = 100;
        let mut late = event("fault.cleared", Some("team-b"));
        late.timestamp = 200;
        late.principal = "token:bob".to_string();
        late.details
            .insert("token_name".to_string(), "bob".to_string());
        let mut other = event("app.deleted", Some("team-a"));
        other.timestamp = 300;
        for record in [early, late, other] {
            assert!(log.submit(record).written().await);
        }
        let query = |filter: AuditFilter| {
            let log = log.clone();
            async move {
                log.query(filter)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|record| record.action)
                    .collect::<Vec<_>>()
            }
        };
        let family = AuditFilter {
            action: Some("fault.*".to_string()),
            ..AuditFilter::default()
        };
        assert_eq!(query(family).await, ["fault.injected", "fault.cleared"]);
        let by_token_name = AuditFilter {
            principal: Some("bob".to_string()),
            ..AuditFilter::default()
        };
        assert_eq!(query(by_token_name).await, ["fault.cleared"]);
        let by_namespace = AuditFilter {
            namespace: Some("team-a".to_string()),
            ..AuditFilter::default()
        };
        assert_eq!(query(by_namespace).await, ["fault.injected", "app.deleted"]);
        let window = AuditFilter {
            since: Some(150),
            until: Some(250),
            ..AuditFilter::default()
        };
        assert_eq!(query(window).await, ["fault.cleared"]);
        let newest_one = AuditFilter {
            limit: Some(1),
            ..AuditFilter::default()
        };
        assert_eq!(query(newest_one).await, ["app.deleted"]);
    }

    #[test]
    fn newest_merges_nodes_by_time_and_keeps_the_latest() {
        let record = |node: &str, sequence: u64, timestamp: u64| AuditRecord {
            sequence,
            timestamp,
            kind: EventKind::Operation,
            severity: EventSeverity::Info,
            action: "app.applied".to_string(),
            principal: "token:alice".to_string(),
            app: None,
            namespace: None,
            node: Some(node.to_string()),
            source_address: None,
            details: BTreeMap::new(),
            message: String::new(),
        };
        let merged = newest(
            vec![
                record("b", 1, 30),
                record("a", 7, 10),
                record("a", 8, 30),
                record("b", 2, 40),
            ],
            3,
        );
        let order: Vec<_> = merged
            .iter()
            .map(|r| (r.node.clone().unwrap(), r.sequence))
            .collect();
        assert_eq!(
            order,
            [
                ("a".to_string(), 8),
                ("b".to_string(), 1),
                ("b".to_string(), 2)
            ]
        );
    }

    #[tokio::test]
    async fn a_request_scope_carries_the_caller_and_collects_notes() {
        let caller: SocketAddr = "198.51.100.4:4000".parse().unwrap();
        let (seen, note) = scope_request(Some(caller), async {
            annotate(AuditNote {
                namespace: Some("team-a".to_string()),
                ..AuditNote::default()
            });
            annotate(AuditNote {
                details: BTreeMap::from([("node".to_string(), "node-2".to_string())]),
                ..AuditNote::default()
            });
            caller_address()
        })
        .await;
        assert_eq!(seen, Some(caller));
        assert_eq!(note.namespace.as_deref(), Some("team-a"));
        assert_eq!(note.details.get("node").map(String::as_str), Some("node-2"));
        // Outside a request there is no caller, and notes go nowhere.
        assert_eq!(caller_address(), None);
        annotate(AuditNote::default());
    }

    #[test]
    fn redaction_leaves_ordinary_text_alone() {
        assert_eq!(
            redact_secrets("deleted web in team-a"),
            "deleted web in team-a"
        );
        assert_eq!(
            redact_secrets("token rbrg_0123abcd, then more"),
            "token rbrg_[redacted], then more"
        );
    }
}
