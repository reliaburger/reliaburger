//! Derived, indexed task detail. The checksummed ledger remains authoritative.
//! Every index write follows the ledger's fsync; acknowledgements wait for
//! both. Reopening a writer rebuilds the index from valid ledger records.
//!
//! The index is a dense file with one fixed-size slot per task index, so a
//! task's outcome lives at byte `index * SLOT_BYTES` and a page of indexes is
//! one contiguous read. A slot holds the ledger record plus a 16-bit check;
//! an all-zero slot means "no outcome here". Slots a node never wrote stay
//! holes in a sparse file, so a node that ran a third of an array pays for
//! about a third of it on filesystems with sparse files (ext4, XFS, APFS).
//!
//! It replaced a redb B-tree that cost about 87 bytes per task on disk, four
//! times the ledger itself (#678). Detail pages already examine at most
//! [`super::task_array_node::RESULT_PAGE_SPAN`] indexes, so a dense scan of
//! that span is as cheap as a B-tree lookup, and filtering failures inside it
//! needs no second table.
use super::task_executor::{TaskFinal, TaskRecord};
use super::task_ledger::{self, LedgerError};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

/// Bytes per slot: a ledger record plus its check.
pub const SLOT_BYTES: usize = task_ledger::RECORD_BYTES + 2;

/// Slots read at once by a page scan.
const SCAN_SLOTS: u64 = 4096;

fn error(error: impl std::fmt::Display) -> LedgerError {
    std::io::Error::other(error.to_string()).into()
}

fn check(record: &[u8]) -> [u8; 2] {
    // Truncated CRC32: enough to catch a torn slot, which is all it's for.
    let crc = crc32fast::hash(record).to_le_bytes();
    [crc[0], crc[1]]
}

/// Encode one record as a slot.
fn slot(raw: &[u8; task_ledger::RECORD_BYTES]) -> [u8; SLOT_BYTES] {
    let mut slot = [0u8; SLOT_BYTES];
    slot[..task_ledger::RECORD_BYTES].copy_from_slice(raw);
    slot[task_ledger::RECORD_BYTES..].copy_from_slice(&check(raw));
    slot
}

/// Decode one slot; `None` for an empty one, an error for a damaged one.
fn decode_slot(bytes: &[u8], offset: u64) -> Result<Option<TaskRecord>, LedgerError> {
    if bytes.iter().all(|byte| *byte == 0) {
        return Ok(None);
    }
    let (raw, stored) = bytes.split_at(task_ledger::RECORD_BYTES);
    if check(raw) != stored {
        return Err(error(format!(
            "result index slot at byte {offset} is corrupt; recover the worker ledger before reading detail"
        )));
    }
    task_ledger::decode(raw.try_into().map_err(error)?, offset).map(Some)
}

/// Durable lookup table for one array, shared by writer and API readers.
pub struct TaskResultIndex {
    file: File,
}

impl TaskResultIndex {
    /// Open the derived index, creating it if this is a new array.
    pub fn open(path: &Path) -> Result<Arc<Self>, LedgerError> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        Ok(Arc::new(Self { file }))
    }

    /// Reconstruct from the ledger; a partial index write cannot invent
    /// outcomes or survive after its source block has been discarded.
    pub fn rebuild(&self, ledger: &Path) -> Result<(), LedgerError> {
        self.file.set_len(0)?;
        task_ledger::scan(ledger, |record| {
            let mut raw = Vec::with_capacity(task_ledger::RECORD_BYTES);
            task_ledger::encode(&record, &mut raw);
            self.put(&record, raw.as_slice().try_into().map_err(error)?)
        })?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Write one record unless the slot already holds a newer grant's.
    fn put(
        &self,
        record: &TaskRecord,
        raw: &[u8; task_ledger::RECORD_BYTES],
    ) -> Result<(), LedgerError> {
        let offset = u64::from(record.index) * SLOT_BYTES as u64;
        let mut existing = [0u8; SLOT_BYTES];
        let read = read_full(&self.file, &mut existing, offset)?;
        // A short read is past the end of the file: an empty slot. A damaged
        // slot can only be an unacknowledged write, so it is overwritten.
        if read == SLOT_BYTES
            && let Ok(Some(old)) = decode_slot(&existing, offset)
            && old.grant_attempt > record.grant_attempt
        {
            return Ok(());
        }
        self.file.write_all_at(&slot(raw), offset)?;
        Ok(())
    }

    /// Index already checksummed terminal records, with one fsync.
    pub fn append(&self, bytes: &[u8]) -> Result<(), LedgerError> {
        for raw in bytes.as_chunks::<{ task_ledger::RECORD_BYTES }>().0 {
            let record = task_ledger::decode(raw, 0)?;
            self.put(&record, raw)?;
        }
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
        let slots = self.file.metadata()?.len() / SLOT_BYTES as u64;
        let end = u64::from(end).min(slots);
        let mut records = Vec::new();
        let mut at = u64::from(start);
        let mut buffer = Vec::new();
        while at < end && records.len() < limit {
            let count = (end - at).min(SCAN_SLOTS);
            let length = usize::try_from(count).map_err(error)? * SLOT_BYTES;
            buffer.resize(length, 0);
            let offset = at * SLOT_BYTES as u64;
            let read = read_full(&self.file, &mut buffer, offset)?;
            for (position, bytes) in buffer[..read]
                .as_chunks::<SLOT_BYTES>()
                .0
                .iter()
                .enumerate()
            {
                let slot_offset = offset + (position * SLOT_BYTES) as u64;
                let Some(record) = decode_slot(bytes, slot_offset)? else {
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
