//! The node's durable record of task-array outcomes.
//!
//! Every finished task becomes one fixed-size record in an append-only
//! file. Records are written in blocks, each with a CRC32, and the file
//! is fsynced as a *group*: at most once per [`GroupCommit::interval`] or
//! every [`GroupCommit::max_records`] records, whichever comes first. A
//! node running thousands of tasks a second therefore pays for ten fsyncs
//! a second, not thousands.
//!
//! A chunk is reported to the leader only after its last record is
//! durable. After a crash, [`replay`] reads the file back and says which
//! tasks already finished; anything else in a chunk the node still holds
//! runs again. That's at-least-once execution, on purpose: a task that
//! finished but whose record didn't reach the disk runs twice.
//!
//! On-disk layout (little-endian):
//!
//! ```text
//! file   = MAGIC (8 bytes) block*
//! block  = record_count: u32, crc32(records): u32, record * record_count
//! record = index: u32, attempts: u8, outcome: u8, exit_code: i32, run_ms: u32
//! ```
//!
//! `exit_code` holds `i32::MIN` for "no exit status"; that sentinel never
//! leaves this module.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use super::task_executor::{TaskFinal, TaskRecord};
use crate::meat::index_set::IndexRangeSet;
use crate::meat::task_array::{ChunkId, TaskArraySpec};

/// Identifies the file format.
pub const MAGIC: &[u8; 8] = b"RBTASKL1";

/// Bytes per record.
pub const RECORD_BYTES: usize = 14;

/// Bytes of block header.
const BLOCK_HEADER_BYTES: usize = 8;

/// Most records one block may claim; anything larger is corruption.
pub const MAX_BLOCK_RECORDS: u32 = 1 << 20;

const NO_EXIT_CODE: i32 = i32::MIN;

/// Why the ledger couldn't be read or written.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("ledger I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("{path} isn't a task ledger (bad magic)")]
    BadMagic { path: PathBuf },
    #[error("ledger block at byte {offset} is corrupt and isn't the last block")]
    Corrupt { offset: u64 },
    #[error("ledger record at byte {offset} has an unknown outcome {outcome}")]
    BadRecord { offset: u64, outcome: u8 },
    #[error("the ledger writer has stopped")]
    WriterStopped,
}

/// When buffered records must be written and fsynced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupCommit {
    /// Longest a record waits for its fsync.
    pub interval: Duration,
    /// Most records buffered before an early fsync.
    pub max_records: usize,
}

impl Default for GroupCommit {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(100),
            max_records: 4096,
        }
    }
}

impl GroupCommit {
    /// Whether `pending` records, the oldest waiting `waited`, must be
    /// flushed now.
    pub fn is_due(&self, pending: usize, waited: Duration) -> bool {
        pending > 0 && (pending >= self.max_records || waited >= self.interval)
    }
}

/// An open ledger file, appended to in blocks.
#[derive(Debug)]
pub struct Ledger {
    file: File,
    pending: Vec<u8>,
    pending_records: u32,
    syncs: u64,
}

impl Ledger {
    /// Open the ledger at `path`, creating it (with its magic) if it's
    /// missing. An existing file must start with the magic. Blocking I/O.
    pub fn open(path: &Path) -> Result<Self, LedgerError> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        let length = file.metadata()?.len();
        if length == 0 {
            file.write_all(MAGIC)?;
            file.sync_all()?;
        } else {
            let mut magic = [0u8; 8];
            let mut reader = File::open(path)?;
            if length < 8 || reader.read_exact(&mut magic).is_err() || &magic != MAGIC {
                return Err(LedgerError::BadMagic {
                    path: path.to_path_buf(),
                });
            }
        }
        Ok(Self {
            file,
            pending: Vec::new(),
            pending_records: 0,
            syncs: 0,
        })
    }

    /// Buffer records; nothing is durable until [`Self::flush`].
    pub fn append(&mut self, records: &[TaskRecord]) {
        for record in records {
            encode(record, &mut self.pending);
        }
        self.pending_records += records.len() as u32;
    }

    /// Records buffered and not yet durable.
    pub fn pending(&self) -> usize {
        self.pending_records as usize
    }

    /// Write the buffered records as one block and fsync. Does nothing
    /// (and doesn't fsync) when nothing is buffered. Blocking I/O.
    pub fn flush(&mut self) -> Result<(), LedgerError> {
        if self.pending_records == 0 {
            return Ok(());
        }
        let mut block = Vec::with_capacity(BLOCK_HEADER_BYTES + self.pending.len());
        block.extend_from_slice(&self.pending_records.to_le_bytes());
        block.extend_from_slice(&crc32fast::hash(&self.pending).to_le_bytes());
        block.extend_from_slice(&self.pending);
        self.file.write_all(&block)?;
        self.file.sync_data()?;
        self.syncs += 1;
        self.pending.clear();
        self.pending_records = 0;
        Ok(())
    }

    /// How many fsyncs this ledger has done.
    pub fn syncs(&self) -> u64 {
        self.syncs
    }
}

fn encode(record: &TaskRecord, out: &mut Vec<u8>) {
    let outcome: u8 = match record.outcome {
        TaskFinal::Succeeded => 1,
        TaskFinal::Failed => 2,
        TaskFinal::NotRun => 3,
    };
    out.extend_from_slice(&record.index.to_le_bytes());
    out.push(record.attempts);
    out.push(outcome);
    out.extend_from_slice(&record.exit_code.unwrap_or(NO_EXIT_CODE).to_le_bytes());
    out.extend_from_slice(&record.run_ms.to_le_bytes());
}

fn decode(bytes: &[u8; RECORD_BYTES], offset: u64) -> Result<TaskRecord, LedgerError> {
    let u32_at =
        |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let outcome = match bytes[5] {
        1 => TaskFinal::Succeeded,
        2 => TaskFinal::Failed,
        3 => TaskFinal::NotRun,
        other => {
            return Err(LedgerError::BadRecord {
                offset,
                outcome: other,
            });
        }
    };
    let exit_code = u32_at(6) as i32;
    Ok(TaskRecord {
        index: u32_at(0),
        attempts: bytes[4],
        outcome,
        exit_code: (exit_code != NO_EXIT_CODE).then_some(exit_code),
        run_ms: u32_at(10),
        output: None,
    })
}

/// What a ledger held when it was read back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplayedLedger {
    /// Every durable record, in file order. A task can appear more than
    /// once (a re-run after a crash); the last record wins.
    pub records: Vec<TaskRecord>,
    /// Indices with a terminal record (succeeded or failed).
    pub finished: IndexRangeSet,
    /// Whether a torn final block was ignored.
    pub torn_tail: bool,
}

impl ReplayedLedger {
    /// The tasks of `chunk` that still need to run.
    pub fn remaining(&self, spec: &TaskArraySpec, chunk: ChunkId) -> IndexRangeSet {
        let Some(range) = spec.chunk_range(chunk) else {
            return IndexRangeSet::new();
        };
        let mut remaining = IndexRangeSet::from_range(range);
        for finished in self.finished.ranges() {
            remaining.remove_range(finished);
        }
        remaining
    }
}

/// Read a ledger back. A block cut short by a crash at the end of the
/// file is ignored (its records weren't acknowledged as durable); damage
/// anywhere else is an error rather than a silent skip, because skipping
/// would re-run or lose finished tasks without saying so. Blocking I/O.
pub fn replay(path: &Path) -> Result<ReplayedLedger, LedgerError> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
        return Err(LedgerError::BadMagic {
            path: path.to_path_buf(),
        });
    }
    let mut replayed = ReplayedLedger::default();
    let mut at = MAGIC.len();
    while at < bytes.len() {
        let offset = at as u64;
        let Some(header) = bytes.get(at..at + BLOCK_HEADER_BYTES) else {
            replayed.torn_tail = true;
            break;
        };
        let count = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let crc = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if count == 0 || count > MAX_BLOCK_RECORDS {
            return torn_or_corrupt(&mut replayed, at, bytes.len(), offset);
        }
        let body_start = at + BLOCK_HEADER_BYTES;
        let body_end = body_start + count as usize * RECORD_BYTES;
        let Some(body) = bytes.get(body_start..body_end) else {
            replayed.torn_tail = true;
            break;
        };
        if crc32fast::hash(body) != crc {
            // A full-length block with a bad checksum is torn only if
            // nothing follows it.
            if body_end == bytes.len() {
                replayed.torn_tail = true;
                break;
            }
            return Err(LedgerError::Corrupt { offset });
        }
        let (raw_records, _) = body.as_chunks::<RECORD_BYTES>();
        for (position, raw) in raw_records.iter().enumerate() {
            let record_offset = (body_start + position * RECORD_BYTES) as u64;
            let record = decode(raw, record_offset)?;
            match record.outcome {
                TaskFinal::Succeeded | TaskFinal::Failed => {
                    replayed.finished.insert(record.index);
                }
                TaskFinal::NotRun => {}
            }
            replayed.records.push(record);
        }
        at = body_end;
    }
    Ok(replayed)
}

fn torn_or_corrupt(
    replayed: &mut ReplayedLedger,
    at: usize,
    length: usize,
    offset: u64,
) -> Result<ReplayedLedger, LedgerError> {
    // A nonsense header is a torn tail only if the rest of the file is
    // too short to have held a real block after it.
    if length - at < BLOCK_HEADER_BYTES + RECORD_BYTES {
        replayed.torn_tail = true;
        return Ok(std::mem::take(replayed));
    }
    Err(LedgerError::Corrupt { offset })
}

/// A request to the ledger writer.
#[derive(Debug)]
pub struct AppendRequest {
    /// Records to make durable.
    pub records: Vec<TaskRecord>,
    /// Answered once the records are durable (or the write failed).
    pub durable: oneshot::Sender<Result<(), String>>,
}

/// A handle for appending to a ledger owned by a background writer.
#[derive(Debug, Clone)]
pub struct LedgerHandle {
    requests: mpsc::Sender<AppendRequest>,
}

impl LedgerHandle {
    /// Append records and wait until they're durable.
    pub async fn append(&self, records: Vec<TaskRecord>) -> Result<(), LedgerError> {
        let (durable, done) = oneshot::channel();
        self.requests
            .send(AppendRequest { records, durable })
            .await
            .map_err(|_| LedgerError::WriterStopped)?;
        match done.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(reason)) => Err(LedgerError::Io(std::io::Error::other(reason))),
            Err(_) => Err(LedgerError::WriterStopped),
        }
    }
}

/// Start the group-commit writer for `ledger`. Appends from every chunk
/// share its fsyncs. The writer stops when every handle is dropped, after
/// flushing what it holds, and returns the ledger.
pub fn spawn_writer(
    ledger: Ledger,
    policy: GroupCommit,
) -> (
    LedgerHandle,
    tokio::task::JoinHandle<Result<Ledger, LedgerError>>,
) {
    let (requests, receiver) = mpsc::channel(1024);
    let task = tokio::spawn(run_writer(ledger, policy, receiver));
    (LedgerHandle { requests }, task)
}

async fn run_writer(
    mut ledger: Ledger,
    policy: GroupCommit,
    mut receiver: mpsc::Receiver<AppendRequest>,
) -> Result<Ledger, LedgerError> {
    let mut waiting: Vec<oneshot::Sender<Result<(), String>>> = Vec::new();
    let mut oldest: Option<Instant> = None;
    loop {
        let deadline = oldest.map(|since| since + policy.interval);
        let request = match deadline {
            Some(deadline) => tokio::select! {
                request = receiver.recv() => request,
                () = tokio::time::sleep_until(deadline.into()) => {
                    ledger = commit(ledger, &mut waiting).await?;
                    oldest = None;
                    continue;
                }
            },
            None => receiver.recv().await,
        };
        let Some(request) = request else {
            break;
        };
        ledger.append(&request.records);
        waiting.push(request.durable);
        let since = *oldest.get_or_insert_with(Instant::now);
        if policy.is_due(ledger.pending(), since.elapsed()) {
            ledger = commit(ledger, &mut waiting).await?;
            oldest = None;
        }
    }
    commit(ledger, &mut waiting).await
}

/// Flush off the async runtime, then answer everyone waiting. A write
/// error is reported to the waiters and the ledger carries on; losing the
/// ledger itself (the blocking task panicked) stops the writer, and every
/// later append then fails with [`LedgerError::WriterStopped`].
async fn commit(
    mut ledger: Ledger,
    waiting: &mut Vec<oneshot::Sender<Result<(), String>>>,
) -> Result<Ledger, LedgerError> {
    let flushed = tokio::task::spawn_blocking(move || {
        let result = ledger.flush().map_err(|e| e.to_string());
        (ledger, result)
    })
    .await;
    let (ledger, result) = match flushed {
        Ok(done) => done,
        Err(error) => {
            for waiter in waiting.drain(..) {
                let _ = waiter.send(Err(error.to_string()));
            }
            return Err(LedgerError::WriterStopped);
        }
    };
    for waiter in waiting.drain(..) {
        let _ = waiter.send(result.clone());
    }
    Ok(ledger)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(index: u32, outcome: TaskFinal) -> TaskRecord {
        TaskRecord {
            index,
            attempts: 1,
            outcome,
            exit_code: match outcome {
                TaskFinal::Succeeded => Some(0),
                TaskFinal::Failed => Some(-9),
                TaskFinal::NotRun => None,
            },
            run_ms: index.wrapping_mul(3),
            output: None,
        }
    }

    fn path_in(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("ledger")
    }

    #[test]
    fn records_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(&path_in(&dir)).unwrap();
        let written = vec![
            record(0, TaskFinal::Succeeded),
            record(1, TaskFinal::Failed),
            record(2, TaskFinal::NotRun),
            TaskRecord {
                attempts: 3,
                run_ms: u32::MAX,
                ..record(u32::MAX, TaskFinal::Failed)
            },
        ];
        ledger.append(&written);
        ledger.flush().unwrap();
        ledger.append(&[record(7, TaskFinal::Succeeded)]);
        ledger.flush().unwrap();

        let replayed = replay(&path_in(&dir)).unwrap();
        let mut expected = written;
        expected.push(record(7, TaskFinal::Succeeded));
        assert_eq!(replayed.records, expected);
        assert!(!replayed.torn_tail);
        assert!(replayed.finished.contains(0));
        assert!(replayed.finished.contains(1));
        assert!(
            !replayed.finished.contains(2),
            "not-run tasks aren't finished"
        );
    }

    #[test]
    fn reopening_appends_after_existing_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = Ledger::open(&path_in(&dir)).unwrap();
        first.append(&[record(1, TaskFinal::Succeeded)]);
        first.flush().unwrap();
        drop(first);
        let mut second = Ledger::open(&path_in(&dir)).unwrap();
        second.append(&[record(2, TaskFinal::Succeeded)]);
        second.flush().unwrap();
        assert_eq!(replay(&path_in(&dir)).unwrap().records.len(), 2);
    }

    #[test]
    fn a_file_without_the_magic_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(path_in(&dir), b"something else entirely").unwrap();
        assert!(matches!(
            Ledger::open(&path_in(&dir)),
            Err(LedgerError::BadMagic { .. })
        ));
        assert!(matches!(
            replay(&path_in(&dir)),
            Err(LedgerError::BadMagic { .. })
        ));
    }

    #[test]
    fn flushing_nothing_doesnt_fsync() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(&path_in(&dir)).unwrap();
        ledger.flush().unwrap();
        assert_eq!(ledger.syncs(), 0);
    }

    #[test]
    fn a_torn_final_block_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(&path_in(&dir)).unwrap();
        ledger.append(&[record(1, TaskFinal::Succeeded)]);
        ledger.flush().unwrap();
        ledger.append(&[
            record(2, TaskFinal::Succeeded),
            record(3, TaskFinal::Succeeded),
        ]);
        ledger.flush().unwrap();
        let full = std::fs::read(path_in(&dir)).unwrap();

        // Cut anywhere inside the second block: header, body, last byte.
        let second_block = MAGIC.len() + BLOCK_HEADER_BYTES + RECORD_BYTES;
        for cut in [second_block + 3, second_block + 10, full.len() - 1] {
            std::fs::write(path_in(&dir), &full[..cut]).unwrap();
            let replayed = replay(&path_in(&dir)).unwrap();
            assert_eq!(
                replayed.records,
                vec![record(1, TaskFinal::Succeeded)],
                "cut at {cut}"
            );
            assert!(replayed.torn_tail);
        }

        // A full-length last block with garbage in it is torn too.
        let mut garbled = full.clone();
        let last = garbled.len() - 2;
        garbled[last] ^= 0xff;
        std::fs::write(path_in(&dir), &garbled).unwrap();
        let replayed = replay(&path_in(&dir)).unwrap();
        assert!(replayed.torn_tail);
        assert_eq!(replayed.records.len(), 1);
    }

    #[test]
    fn corruption_before_the_last_block_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(&path_in(&dir)).unwrap();
        for i in 0..3 {
            ledger.append(&[record(i, TaskFinal::Succeeded)]);
            ledger.flush().unwrap();
        }
        let mut bytes = std::fs::read(path_in(&dir)).unwrap();
        // Flip a bit in the first block's record.
        bytes[MAGIC.len() + BLOCK_HEADER_BYTES] ^= 0x01;
        std::fs::write(path_in(&dir), &bytes).unwrap();
        assert!(matches!(
            replay(&path_in(&dir)),
            Err(LedgerError::Corrupt { offset: 8 })
        ));
    }

    #[test]
    fn replay_says_which_tasks_of_a_chunk_still_need_to_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(&path_in(&dir)).unwrap();
        ledger.append(&[
            record(100, TaskFinal::Succeeded),
            record(101, TaskFinal::Failed),
            record(102, TaskFinal::NotRun),
            record(150, TaskFinal::Succeeded),
        ]);
        ledger.flush().unwrap();
        let replayed = replay(&path_in(&dir)).unwrap();
        let spec = TaskArraySpec {
            chunk_size: 100,
            ..TaskArraySpec::with_count(1000)
        };
        let mut expected = IndexRangeSet::from_range(102..=199);
        expected.remove(150);
        assert_eq!(replayed.remaining(&spec, ChunkId(1)), expected);
        assert_eq!(
            replayed.remaining(&spec, ChunkId(0)),
            IndexRangeSet::from_range(0..=99)
        );
        assert!(replayed.remaining(&spec, ChunkId(99)).is_empty());
    }

    #[test]
    fn a_million_records_fit_the_disk_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(&path_in(&dir)).unwrap();
        let batch: Vec<TaskRecord> = (0..4096).map(|i| record(i, TaskFinal::Succeeded)).collect();
        for block in 0..245u32 {
            let shifted: Vec<TaskRecord> = batch
                .iter()
                .map(|r| TaskRecord {
                    index: r.index + block * 4096,
                    ..r.clone()
                })
                .collect();
            ledger.append(&shifted);
            ledger.flush().unwrap();
        }
        let bytes = std::fs::metadata(path_in(&dir)).unwrap().len();
        assert!(bytes <= 16 * 1024 * 1024, "{bytes} bytes");
        let replayed = replay(&path_in(&dir)).unwrap();
        assert_eq!(replayed.records.len(), 245 * 4096);
        assert_eq!(replayed.finished.range_count(), 1);
    }

    #[test]
    fn group_commit_is_due_on_size_or_age() {
        let policy = GroupCommit::default();
        assert!(!policy.is_due(0, Duration::from_secs(9)));
        assert!(!policy.is_due(10, Duration::from_millis(50)));
        assert!(policy.is_due(10, Duration::from_millis(100)));
        assert!(policy.is_due(4096, Duration::ZERO));
    }

    #[tokio::test]
    async fn concurrent_appends_share_fsyncs() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&path_in(&dir)).unwrap();
        let policy = GroupCommit {
            interval: Duration::from_millis(200),
            max_records: 1_000_000,
        };
        let (handle, writer) = spawn_writer(ledger, policy);
        let mut appends = tokio::task::JoinSet::new();
        for chunk in 0..20u32 {
            let handle = handle.clone();
            appends.spawn(async move {
                let records = (0..10)
                    .map(|i| record(chunk * 10 + i, TaskFinal::Succeeded))
                    .collect();
                handle.append(records).await
            });
        }
        while let Some(done) = appends.join_next().await {
            done.unwrap().unwrap();
        }
        drop(handle);
        let ledger = writer.await.unwrap().unwrap();
        // Twenty appends inside one interval: one or two fsyncs, not twenty.
        assert!(ledger.syncs() <= 2, "{} fsyncs", ledger.syncs());
        let replayed = replay(&path_in(&dir)).unwrap();
        assert_eq!(replayed.finished, IndexRangeSet::from_range(0..=199));
    }

    #[tokio::test]
    async fn a_full_group_commits_early() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&path_in(&dir)).unwrap();
        let policy = GroupCommit {
            interval: Duration::from_secs(3600),
            max_records: 5,
        };
        let (handle, writer) = spawn_writer(ledger, policy);
        let records = (0..5).map(|i| record(i, TaskFinal::Succeeded)).collect();
        // Returns without waiting an hour: five records fill the group.
        tokio::time::timeout(Duration::from_secs(5), handle.append(records))
            .await
            .unwrap()
            .unwrap();
        drop(handle);
        assert_eq!(writer.await.unwrap().unwrap().syncs(), 1);
    }
}
