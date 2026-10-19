//! Derived, indexed task detail. The checksummed ledger remains authoritative.
//! Every index write follows the ledger's fsync; acknowledgements wait for
//! both. Reopening a writer rebuilds the index from valid ledger records.
//!
//! The index is a dense file with one fixed-size slot per task index, so a
//! task's outcome lives at byte `index * slot size` and a page of indexes is
//! one contiguous read. Slots a node never wrote stay holes in a sparse
//! file, so a node that ran a third of an array pays for about a third of it
//! on filesystems with sparse files (ext4, XFS, APFS).
//!
//! It comes in two shapes:
//!
//! - **Live**, while the run is active: [`SLOT_BYTES`] per slot, the ledger
//!   record plus a 16-bit check, because slots are rewritten in place and a
//!   crash can tear one.
//! - **Final**, once the run is terminal: [`compact`] rewrites the ledger's
//!   outcomes into [`FINAL_SLOT_BYTES`]-byte slots in [`RESULTS_FILE`], then
//!   deletes the ledger and the live index (#739). A final slot drops the
//!   task index, because its position says it, and the check, because the
//!   file is written whole, fsynced and renamed into place, so nothing can
//!   tear it. It narrows the grant attempt to 32 bits.
//!
//! Either way an all-zero slot means "no outcome here": every stored
//! outcome byte is non-zero.
//!
//! The live index replaced a redb B-tree that cost about 87 bytes per task on
//! disk, four times the ledger itself (#678). Detail pages already examine at
//! most [`super::task_array_node::RESULT_PAGE_SPAN`] indexes, so a dense scan
//! of that span is as cheap as a B-tree lookup, and filtering failures inside
//! it needs no second table.
use super::task_executor::{TaskFinal, TaskRecord};
use super::task_ledger::{self, LedgerError};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

/// Bytes per live slot: a ledger record plus its check.
pub const SLOT_BYTES: usize = task_ledger::RECORD_BYTES + 2;

/// Bytes per final slot: outcome, attempts, exit code, run time and grant.
pub const FINAL_SLOT_BYTES: usize = 14;

/// Name of a finished run's compacted results, beside its ledger.
pub const RESULTS_FILE: &str = "results";

/// Slots read at once by a page scan.
const SCAN_SLOTS: u64 = 4096;

/// `exit_code` of a final slot without an exit status, as in the ledger.
const NO_EXIT_CODE: i32 = i32::MIN;

fn error(error: impl std::fmt::Display) -> LedgerError {
    std::io::Error::other(error.to_string()).into()
}

fn check(record: &[u8]) -> [u8; 2] {
    // Truncated CRC32: enough to catch a torn slot, which is all it's for.
    let crc = crc32fast::hash(record).to_le_bytes();
    [crc[0], crc[1]]
}

/// How an index lays out its slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotFormat {
    /// Rewritten in place while the run is active.
    Live,
    /// Written once, after the run is terminal.
    Final,
}

impl SlotFormat {
    fn bytes(self) -> usize {
        match self {
            Self::Live => SLOT_BYTES,
            Self::Final => FINAL_SLOT_BYTES,
        }
    }

    /// Append `record` as one slot.
    fn encode(self, record: &TaskRecord, out: &mut Vec<u8>) -> Result<(), LedgerError> {
        match self {
            Self::Live => {
                let start = out.len();
                task_ledger::encode(record, out);
                let crc = check(&out[start..]);
                out.extend_from_slice(&crc);
            }
            Self::Final => {
                let grant = u32::try_from(record.grant_attempt).map_err(|_| {
                    error(format!(
                        "task {} has grant attempt {}, too large for a compact result",
                        record.index, record.grant_attempt
                    ))
                })?;
                out.push(task_ledger::outcome_byte(record.outcome));
                out.push(record.attempts);
                out.extend_from_slice(&record.exit_code.unwrap_or(NO_EXIT_CODE).to_le_bytes());
                out.extend_from_slice(&record.run_ms.to_le_bytes());
                out.extend_from_slice(&grant.to_le_bytes());
            }
        }
        Ok(())
    }

    /// Decode the slot of task `index`, found at byte `offset`: `None` for
    /// an empty one, an error for a damaged one.
    fn decode(
        self,
        bytes: &[u8],
        index: u32,
        offset: u64,
    ) -> Result<Option<TaskRecord>, LedgerError> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Ok(None);
        }
        match self {
            Self::Live => {
                let (raw, stored) = bytes.split_at(task_ledger::RECORD_BYTES);
                if check(raw) != stored {
                    return Err(error(format!(
                        "result index slot at byte {offset} is corrupt; recover the worker ledger before reading detail"
                    )));
                }
                task_ledger::decode(raw.try_into().map_err(error)?, offset).map(Some)
            }
            Self::Final => {
                let u32_at = |at: usize| {
                    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
                };
                let Some(outcome) = task_ledger::outcome_from_byte(bytes[0]) else {
                    return Err(error(format!(
                        "result slot at byte {offset} has an unknown outcome {}; the results file is damaged",
                        bytes[0]
                    )));
                };
                let exit_code = u32_at(2) as i32;
                Ok(Some(TaskRecord {
                    grant_attempt: u64::from(u32_at(10)),
                    index,
                    attempts: bytes[1],
                    outcome,
                    exit_code: (exit_code != NO_EXIT_CODE).then_some(exit_code),
                    run_ms: u32_at(6),
                    output: None,
                    // Results keep attempt counts only, like the ledger.
                    retry_causes: Default::default(),
                }))
            }
        }
    }
}

/// Durable lookup table for one array, shared by writer and API readers.
#[derive(Debug)]
pub struct TaskResultIndex {
    file: File,
    format: SlotFormat,
}

impl TaskResultIndex {
    /// Open the derived index, creating it if this is a new array.
    pub fn open(path: &Path) -> Result<Arc<Self>, LedgerError> {
        Ok(Arc::new(Self::create(path, SlotFormat::Live)?))
    }

    /// Open a finished run's compacted results, written by [`compact`].
    pub fn open_final(path: &Path) -> Result<Arc<Self>, LedgerError> {
        let file = File::open(path)?;
        Ok(Arc::new(Self {
            file,
            format: SlotFormat::Final,
        }))
    }

    /// Whether this is a finished run's compacted results.
    pub fn is_final(&self) -> bool {
        self.format == SlotFormat::Final
    }

    fn create(path: &Path, format: SlotFormat) -> Result<Self, LedgerError> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        Ok(Self { file, format })
    }

    /// Reconstruct from the ledger; a partial index write cannot invent
    /// outcomes or survive after its source block has been discarded.
    pub fn rebuild(&self, ledger: &Path) -> Result<(), LedgerError> {
        self.file.set_len(0)?;
        let mut pending = Vec::new();
        task_ledger::scan(ledger, |record| {
            task_ledger::encode(&record, &mut pending);
            if pending.len() >= SCAN_SLOTS as usize * task_ledger::RECORD_BYTES {
                self.put(&pending)?;
                pending.clear();
            }
            Ok(())
        })?;
        self.put(&pending)?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Write encoded records, each unless its slot already holds a newer
    /// grant's. Records are applied in order, so a later one at the same
    /// grant wins. Each run of adjacent indexes costs one read and one
    /// write; a gap is never filled, so unwritten slots stay holes.
    fn put(&self, bytes: &[u8]) -> Result<(), LedgerError> {
        let mut records = bytes
            .as_chunks::<{ task_ledger::RECORD_BYTES }>()
            .0
            .iter()
            .map(|raw| task_ledger::decode(raw, 0))
            .collect::<Result<Vec<_>, LedgerError>>()?;
        // Stable: equal indexes keep their order.
        records.sort_by_key(|record| record.index);
        let mut start = 0;
        while start < records.len() {
            let mut end = start + 1;
            while end < records.len() && records[end].index <= records[end - 1].index + 1 {
                end += 1;
            }
            self.put_run(&records[start..end])?;
            start = end;
        }
        Ok(())
    }

    /// Apply sorted records whose indexes leave no gap.
    fn put_run(&self, run: &[TaskRecord]) -> Result<(), LedgerError> {
        let (Some(first), Some(last)) = (run.first(), run.last()) else {
            return Ok(());
        };
        let width = self.format.bytes();
        let offset = u64::from(first.index) * width as u64;
        let slots = usize::try_from(last.index - first.index + 1).map_err(error)?;
        let mut buffer = vec![0u8; slots * width];
        // Past the end of the file reads short: those slots are empty.
        read_full(&self.file, &mut buffer, offset)?;
        let mut encoded = Vec::with_capacity(width);
        for record in run {
            let at = usize::try_from(record.index - first.index).map_err(error)? * width;
            let existing = &mut buffer[at..at + width];
            // A damaged slot can only be an unacknowledged write: overwrite it.
            if let Ok(Some(old)) = self
                .format
                .decode(existing, record.index, offset + at as u64)
                && old.grant_attempt > record.grant_attempt
            {
                continue;
            }
            encoded.clear();
            self.format.encode(record, &mut encoded)?;
            existing.copy_from_slice(&encoded);
        }
        self.file.write_all_at(&buffer, offset)?;
        Ok(())
    }

    /// Index already checksummed terminal records, with one fsync.
    pub fn append(&self, bytes: &[u8]) -> Result<(), LedgerError> {
        self.put(bytes)?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Bounded indexed range, in index order: at most `limit` rows, just the
    /// failures when `failed` is set. Reads only the slots in `start..end`.
    pub fn page(
        &self,
        start: u32,
        end: u32,
        failed: bool,
        limit: usize,
    ) -> Result<Vec<TaskRecord>, LedgerError> {
        let width = self.format.bytes();
        let slots = self.file.metadata()?.len() / width as u64;
        let end = u64::from(end).min(slots);
        let mut records = Vec::new();
        let mut at = u64::from(start);
        let mut buffer = Vec::new();
        while at < end && records.len() < limit {
            let count = (end - at).min(SCAN_SLOTS);
            let length = usize::try_from(count).map_err(error)? * width;
            buffer.resize(length, 0);
            let offset = at * width as u64;
            let read = read_full(&self.file, &mut buffer, offset)?;
            for (position, bytes) in buffer[..read - read % width]
                .chunks_exact(width)
                .enumerate()
            {
                let slot_offset = offset + (position * width) as u64;
                // `at` and `position` are below `end`, which came from a u32.
                let index = u32::try_from(at + position as u64).map_err(error)?;
                let Some(record) = self.format.decode(bytes, index, slot_offset)? else {
                    continue;
                };
                if failed && record.outcome != TaskFinal::Failed {
                    continue;
                }
                records.push(record);
                if records.len() >= limit {
                    break;
                }
            }
            at += count;
        }
        Ok(records)
    }
}

/// Keep only what a finished run's readers need: rewrite the outcomes in
/// `ledger` into compact final slots at `results`, then delete the ledger
/// and its live index. Returns the compacted results, or `None` when there
/// is neither a ledger nor results (this node never ran the array).
///
/// Call it only once nothing appends to the ledger any more. The ledger is
/// the source, not the live index: it holds every durable record, even one
/// a crash kept out of the index. Safe to repeat and to interrupt: the
/// results are written to a temporary file, fsynced and renamed into place
/// before anything is deleted, so a crash leaves either the ledger or the
/// complete results, and the next call finishes the job. Blocking I/O.
pub fn compact(ledger: &Path, results: &Path) -> Result<Option<Arc<TaskResultIndex>>, LedgerError> {
    let directory = results
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !results.try_exists()? {
        if !ledger.try_exists()? {
            return Ok(None);
        }
        let temporary = results.with_extension("tmp");
        let written = TaskResultIndex::create(&temporary, SlotFormat::Final)
            .and_then(|index| index.rebuild(ledger));
        if let Err(error) = written {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        std::fs::rename(&temporary, results)?;
        File::open(directory)?.sync_all()?;
    }
    let mut removed = false;
    for leftover in [ledger.to_owned(), ledger.with_extension("index")] {
        match std::fs::remove_file(&leftover) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if removed {
        File::open(directory)?.sync_all()?;
    }
    TaskResultIndex::open_final(results).map(Some)
}

/// Open whatever a reader should page through for a run: its compacted
/// `results` once it has them, otherwise the live index beside `ledger`.
/// `None` when there is neither (this node never ran the array). Opens
/// nothing for writing, so a read racing [`compact`] can't recreate a file
/// compaction just deleted; an index it already opened stays readable after
/// the unlink. Blocking I/O.
pub fn open_for_reading(
    ledger: &Path,
    results: &Path,
) -> Result<Option<Arc<TaskResultIndex>>, LedgerError> {
    let not_found = |error: &LedgerError| matches!(error, LedgerError::Io(io) if io.kind() == std::io::ErrorKind::NotFound);
    let compacted = || match TaskResultIndex::open_final(results) {
        Ok(index) => Ok(Some(index)),
        Err(error) if not_found(&error) => Ok(None),
        Err(error) => Err(error),
    };
    if let Some(index) = compacted()? {
        return Ok(Some(index));
    }
    if !ledger.try_exists()? {
        // Compaction may have finished between the two looks.
        return compacted();
    }
    match File::open(ledger.with_extension("index")) {
        Ok(file) => Ok(Some(Arc::new(TaskResultIndex {
            file,
            format: SlotFormat::Live,
        }))),
        Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => compacted()?
            .ok_or_else(|| {
                error("result index unavailable; recover the worker ledger before reading detail")
            })
            .map(Some),
        Err(error) => Err(error.into()),
    }
}

/// Read as much of `buffer` as the file holds from `offset`.
fn read_full(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read_at(&mut buffer[filled..], offset + filled as u64) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(index: u32, grant_attempt: u64, outcome: TaskFinal) -> TaskRecord {
        TaskRecord {
            grant_attempt,
            index,
            attempts: 1,
            outcome,
            exit_code: Some(i32::from(outcome == TaskFinal::Failed)),
            run_ms: 7,
            output: None,
            retry_causes: Default::default(),
        }
    }

    fn bytes(records: &[TaskRecord]) -> Vec<u8> {
        let mut out = Vec::new();
        for record in records {
            task_ledger::encode(record, &mut out);
        }
        out
    }

    #[test]
    fn the_newest_grant_wins_and_failures_filter_inside_the_span() {
        let dir = tempfile::tempdir().unwrap();
        let index = TaskResultIndex::open(&dir.path().join("index")).unwrap();
        index
            .append(&bytes(&[
                record(3, 2, TaskFinal::Failed),
                record(5, 1, TaskFinal::Succeeded),
                record(9, 1, TaskFinal::Failed),
            ]))
            .unwrap();
        // A late report from an older grant can't overwrite the newer one.
        index
            .append(&bytes(&[record(3, 1, TaskFinal::Succeeded)]))
            .unwrap();

        let all = index.page(0, u32::MAX, false, 100).unwrap();
        assert_eq!(all.iter().map(|r| r.index).collect::<Vec<_>>(), [3, 5, 9]);
        assert_eq!(all[0].outcome, TaskFinal::Failed);
        let failed = index.page(4, 10, true, 100).unwrap();
        assert_eq!(failed.iter().map(|r| r.index).collect::<Vec<_>>(), [9]);
        assert_eq!(index.page(0, 10, false, 2).unwrap().len(), 2);
        assert!(index.page(10, 4, false, 2).unwrap().is_empty());
    }

    /// A ledger at `dir/ledger` (with its live index) holding `blocks`.
    fn ledger_with(dir: &Path, blocks: &[Vec<TaskRecord>]) -> std::path::PathBuf {
        let path = dir.join("ledger");
        let mut ledger = task_ledger::Ledger::open(&path).unwrap();
        for block in blocks {
            ledger.append(block);
            ledger.flush().unwrap();
        }
        path
    }

    fn mixed_outcomes() -> Vec<Vec<TaskRecord>> {
        vec![
            vec![
                record(0, 1, TaskFinal::Succeeded),
                record(1, 1, TaskFinal::Failed),
                TaskRecord {
                    exit_code: None,
                    ..record(2, 1, TaskFinal::NotRun)
                },
                TaskRecord {
                    exit_code: Some(-9),
                    attempts: 3,
                    run_ms: u32::MAX,
                    ..record(3, 1, TaskFinal::Failed)
                },
                // Index 4 never ran here: a hole.
                record(5, 1, TaskFinal::Succeeded),
                record(9000, 7, TaskFinal::Failed),
            ],
            // A newer grant replaces index 1; an older one can't replace 9000.
            vec![
                record(1, 2, TaskFinal::Succeeded),
                record(9000, 6, TaskFinal::Succeeded),
            ],
        ]
    }

    #[test]
    fn a_compacted_run_answers_every_page_the_live_index_did() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger_with(dir.path(), &mixed_outcomes());
        let live = TaskResultIndex::open(&ledger.with_extension("index")).unwrap();
        let queries = [
            (0, u32::MAX, false, 100),
            (0, u32::MAX, true, 100),
            (1, 4, false, 100),
            (0, u32::MAX, false, 2),
            (4, 5, false, 100),
            (9000, 9001, false, 1),
        ];
        let before: Vec<_> = queries
            .iter()
            .map(|&(start, end, failed, limit)| live.page(start, end, failed, limit).unwrap())
            .collect();
        assert_eq!(before[0].len(), 6);
        assert_eq!(before[0][1].grant_attempt, 2, "the newest grant wins");
        assert_eq!(before[0][5].grant_attempt, 7);
        drop(live);

        let results = dir.path().join(RESULTS_FILE);
        let compacted = compact(&ledger, &results).unwrap().unwrap();
        assert!(compacted.is_final());
        let after: Vec<_> = queries
            .iter()
            .map(|&(start, end, failed, limit)| compacted.page(start, end, failed, limit).unwrap())
            .collect();
        assert_eq!(after, before);
        assert!(!ledger.exists(), "the ledger goes once results are durable");
        assert!(!ledger.with_extension("index").exists());
        assert!(!results.with_extension("tmp").exists());
        // One compact slot per index up to the highest one this node ran.
        assert_eq!(
            std::fs::metadata(&results).unwrap().len(),
            9001 * FINAL_SLOT_BYTES as u64
        );
    }

    #[test]
    fn a_reopened_compacted_run_reads_the_same_results() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger_with(dir.path(), &mixed_outcomes());
        let results = dir.path().join(RESULTS_FILE);
        let first = compact(&ledger, &results).unwrap().unwrap();
        let expected = first.page(0, u32::MAX, false, 100).unwrap();
        drop(first);
        // Compacting again (another sync, or a restarted node) only opens it.
        let again = compact(&ledger, &results).unwrap().unwrap();
        assert_eq!(again.page(0, u32::MAX, false, 100).unwrap(), expected);
        let opened = TaskResultIndex::open_final(&results).unwrap();
        assert_eq!(opened.page(0, u32::MAX, false, 100).unwrap(), expected);
    }

    #[test]
    fn compaction_interrupted_by_a_crash_finishes_without_losing_results() {
        let dir = tempfile::tempdir().unwrap();
        let blocks = mixed_outcomes();
        let ledger = ledger_with(dir.path(), &blocks);
        let results = dir.path().join(RESULTS_FILE);
        let expected = TaskResultIndex::open(&ledger.with_extension("index"))
            .unwrap()
            .page(0, u32::MAX, false, 100)
            .unwrap();

        // Crash while writing the temporary file: it's thrown away and
        // compaction starts again from the ledger.
        std::fs::write(results.with_extension("tmp"), b"half a file").unwrap();
        let compacted = compact(&ledger, &results).unwrap().unwrap();
        assert_eq!(compacted.page(0, u32::MAX, false, 100).unwrap(), expected);

        // Crash after the rename but before the ledger was deleted: the
        // results are already complete, so only the leftovers go.
        std::fs::create_dir_all(dir.path().join("again")).unwrap();
        let ledger = ledger_with(
            &dir.path().join("again"),
            &[vec![record(0, 1, TaskFinal::Failed)]],
        );
        let moved = dir.path().join("again").join(RESULTS_FILE);
        std::fs::copy(&results, &moved).unwrap();
        let reopened = compact(&ledger, &moved).unwrap().unwrap();
        assert_eq!(reopened.page(0, u32::MAX, false, 100).unwrap(), expected);
        assert!(!ledger.exists());
        assert!(!ledger.with_extension("index").exists());
    }

    #[test]
    fn a_node_that_never_ran_the_array_has_nothing_to_compact() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger");
        let results = dir.path().join(RESULTS_FILE);
        assert!(compact(&ledger, &results).unwrap().is_none());
        assert!(!results.exists());
    }

    #[test]
    fn a_grant_too_large_for_a_compact_slot_keeps_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let huge = u64::from(u32::MAX) + 1;
        let ledger = ledger_with(dir.path(), &[vec![record(0, huge, TaskFinal::Failed)]]);
        let results = dir.path().join(RESULTS_FILE);
        let error = compact(&ledger, &results).unwrap_err().to_string();
        assert!(error.contains("grant"), "{error}");
        assert!(!results.exists());
        assert!(!results.with_extension("tmp").exists());
        // Nothing was lost: the live index still answers.
        let live = TaskResultIndex::open(&ledger.with_extension("index")).unwrap();
        assert_eq!(live.page(0, 1, false, 1).unwrap()[0].grant_attempt, huge);
    }

    #[test]
    fn a_damaged_compact_slot_is_an_explicit_error() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger_with(dir.path(), &mixed_outcomes());
        let results = dir.path().join(RESULTS_FILE);
        drop(compact(&ledger, &results).unwrap());
        let mut raw = std::fs::read(&results).unwrap();
        raw[0] = 0x7f; // an outcome no record has
        std::fs::write(&results, raw).unwrap();
        let index = TaskResultIndex::open_final(&results).unwrap();
        assert!(index.page(0, 4, false, 10).is_err());
    }

    #[test]
    fn a_damaged_slot_is_an_explicit_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index");
        let index = TaskResultIndex::open(&path).unwrap();
        index
            .append(&bytes(&[record(1, 1, TaskFinal::Succeeded)]))
            .unwrap();
        let mut raw = std::fs::read(&path).unwrap();
        raw[SLOT_BYTES + 6] ^= 0xff;
        std::fs::write(&path, raw).unwrap();
        let error = index.page(0, 4, false, 10).unwrap_err().to_string();
        assert!(error.contains("corrupt"), "{error}");
    }
}
