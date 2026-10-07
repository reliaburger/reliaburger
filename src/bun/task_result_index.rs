//! Derived, indexed task detail. The checksummed ledger remains authoritative.
//! Every index transaction follows the ledger's fsync; acknowledgements wait
//! for both. Reopening a writer rebuilds the index from valid ledger records.
use super::task_executor::{TaskFinal, TaskRecord};
use super::task_ledger::{self, LedgerError};
use redb::{Database, ReadableTable, TableDefinition};
use std::path::Path;
use std::sync::Arc;
// redb otherwise allows 1 GiB per database. A worker can retain many
// profile indexes, so page reads and group commits share this small cache.
const INDEX_CACHE_BYTES: usize = 1 << 20;
const RESULTS: TableDefinition<u32, &[u8]> = TableDefinition::new("results");
const FAILURES: TableDefinition<u32, &[u8]> = TableDefinition::new("failures");
fn error(error: impl std::fmt::Display) -> LedgerError {
    std::io::Error::other(error.to_string()).into()
}

/// Durable lookup tables for one array, shared by writer and API readers.
pub struct TaskResultIndex {
    database: Database,
}
impl TaskResultIndex {
    /// Open the derived index, creating it if this is a new array.
    pub fn open(path: &Path) -> Result<Arc<Self>, LedgerError> {
        Ok(Arc::new(Self {
            database: Database::builder()
                .set_cache_size(INDEX_CACHE_BYTES)
                .create(path)
                .map_err(error)?,
        }))
    }
    /// Reconstruct from the ledger; a partial index transaction cannot invent
    /// outcomes or survive after its source block has been discarded.
    pub fn rebuild(&self, ledger: &Path) -> Result<(), LedgerError> {
        let transaction = self.database.begin_write().map_err(error)?;
        transaction.delete_table(RESULTS).map_err(error)?;
        transaction.delete_table(FAILURES).map_err(error)?;
        {
            let mut results = transaction.open_table(RESULTS).map_err(error)?;
            let mut failures = transaction.open_table(FAILURES).map_err(error)?;
            task_ledger::scan(ledger, |record| {
                if let Some(old) = results.get(record.index).map_err(error)? {
                    let old = task_ledger::decode(old.value().try_into().map_err(error)?, 0)?;
                    if old.grant_attempt > record.grant_attempt {
                        return Ok(());
                    }
                }
                let mut bytes = Vec::with_capacity(task_ledger::RECORD_BYTES);
                task_ledger::encode(&record, &mut bytes);
                results
                    .insert(record.index, bytes.as_slice())
                    .map_err(error)?;
                if record.outcome == TaskFinal::Failed {
                    failures
                        .insert(record.index, bytes.as_slice())
                        .map_err(error)?;
                } else {
                    failures.remove(record.index).map_err(error)?;
                }
                Ok(())
            })?;
        }
        transaction.commit().map_err(error)
    }
    /// Group commit already checksummed terminal records.
    pub fn append(&self, bytes: &[u8]) -> Result<(), LedgerError> {
        let transaction = self.database.begin_write().map_err(error)?;
        {
            let mut results = transaction.open_table(RESULTS).map_err(error)?;
            let mut failures = transaction.open_table(FAILURES).map_err(error)?;
            for raw in bytes.as_chunks::<{ task_ledger::RECORD_BYTES }>().0 {
                let record = task_ledger::decode(raw, 0)?;
                if let Some(old) = results.get(record.index).map_err(error)? {
                    let old = task_ledger::decode(old.value().try_into().map_err(error)?, 0)?;
                    if old.grant_attempt > record.grant_attempt {
                        continue;
                    }
                }
                results
                    .insert(record.index, raw.as_slice())
                    .map_err(error)?;
                if record.outcome == TaskFinal::Failed {
                    failures
                        .insert(record.index, raw.as_slice())
                        .map_err(error)?;
                } else {
                    failures.remove(record.index).map_err(error)?;
                }
            }
        }
        transaction.commit().map_err(error)
    }
    /// Bounded indexed range; no allocation or scan of historical tasks.
    pub fn page(
        &self,
        start: u32,
        end: u32,
        failed: bool,
        limit: usize,
    ) -> Result<Vec<TaskRecord>, LedgerError> {
        if start >= end {
            return Ok(Vec::new());
        }
        let transaction = self.database.begin_read().map_err(error)?;
        let table = transaction
            .open_table(if failed { FAILURES } else { RESULTS })
            .map_err(error)?;
        let mut records = Vec::new();
        for row in table.range(start..end).map_err(error)?.take(limit) {
            let (_, value) = row.map_err(error)?;
            let raw = value.value().try_into().map_err(error)?;
            records.push(task_ledger::decode(raw, 0)?);
        }
        Ok(records)
    }
}
