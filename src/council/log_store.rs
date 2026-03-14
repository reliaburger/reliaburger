//! In-memory Raft log storage.
//!
//! Stores log entries in a `BTreeMap<u64, Entry>` keyed by log index.
//! Durable enough for testing; production would back this with disk.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::ops::RangeBounds;
use std::sync::Arc;

use openraft::storage::{LogFlushed, LogState, RaftLogStorage};
use openraft::{Entry, LogId, RaftLogReader, StorageError, Vote};
use tokio::sync::Mutex;

use super::types::TypeConfig;

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct LogStoreInner {
    vote: Option<Vote<u64>>,
    committed: Option<LogId<u64>>,
    log: BTreeMap<u64, Entry<TypeConfig>>,
    last_purged_log_id: Option<LogId<u64>>,
}

impl LogStoreInner {
    fn get_log_state(&self) -> LogState<TypeConfig> {
        let last_log_id = self
            .log
            .last_key_value()
            .map(|(_, entry)| entry.log_id)
            .or(self.last_purged_log_id);
        LogState {
            last_purged_log_id: self.last_purged_log_id,
            last_log_id,
        }
    }

    fn append_entries(&mut self, entries: impl IntoIterator<Item = Entry<TypeConfig>>) {
        for entry in entries {
            self.log.insert(entry.log_id.index, entry);
        }
    }

    fn read_range(&self, range: impl RangeBounds<u64>) -> Vec<Entry<TypeConfig>> {
        self.log
            .range(range)
            .map(|(_, entry)| entry.clone())
            .collect()
    }

    fn truncate_since(&mut self, index: u64) {
        let to_remove: Vec<u64> = self.log.range(index..).map(|(k, _)| *k).collect();
        for k in to_remove {
            self.log.remove(&k);
        }
    }

    fn purge_up_to(&mut self, log_id: LogId<u64>) {
        self.last_purged_log_id = Some(log_id);
        let to_remove: Vec<u64> = self.log.range(..=log_id.index).map(|(k, _)| *k).collect();
        for k in to_remove {
            self.log.remove(&k);
        }
    }
}

// ---------------------------------------------------------------------------
// MemLogStore
// ---------------------------------------------------------------------------

/// In-memory Raft log and vote storage.
///
/// All state lives behind `Arc<Mutex<_>>` so the log reader (used by
/// replication tasks) can share access with the main Raft task.
#[derive(Debug, Clone, Default)]
pub struct MemLogStore {
    inner: Arc<Mutex<LogStoreInner>>,
}

impl MemLogStore {
    /// Create a new empty log store.
    pub fn new() -> Self {
        Self::default()
    }
}

// ---------------------------------------------------------------------------
// MemLogReader
// ---------------------------------------------------------------------------

/// Read-only handle into the log store.
///
/// Cloned from `MemLogStore`, shares the same `Arc<Mutex<_>>`.
#[derive(Debug, Clone)]
pub struct MemLogReader {
    inner: Arc<Mutex<LogStoreInner>>,
}

impl RaftLogReader<TypeConfig> for MemLogReader {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<u64>> {
        let guard = self.inner.lock().await;
        Ok(guard.read_range(range))
    }
}

// ---------------------------------------------------------------------------
// RaftLogReader for MemLogStore
// ---------------------------------------------------------------------------

impl RaftLogReader<TypeConfig> for MemLogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<u64>> {
        let guard = self.inner.lock().await;
        Ok(guard.read_range(range))
    }
}

// ---------------------------------------------------------------------------
// RaftLogStorage for MemLogStore
// ---------------------------------------------------------------------------

impl RaftLogStorage<TypeConfig> for MemLogStore {
    type LogReader = MemLogReader;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<u64>> {
        let guard = self.inner.lock().await;
        Ok(guard.get_log_state())
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        MemLogReader {
            inner: Arc::clone(&self.inner),
        }
    }

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<u64>> {
        let mut guard = self.inner.lock().await;
        guard.vote = Some(*vote);
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
        let guard = self.inner.lock().await;
        Ok(guard.vote)
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<u64>>,
    ) -> Result<(), StorageError<u64>> {
        let mut guard = self.inner.lock().await;
        guard.committed = committed;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<u64>>, StorageError<u64>> {
        let guard = self.inner.lock().await;
        Ok(guard.committed)
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError<u64>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + Send,
        I::IntoIter: Send,
    {
        let mut guard = self.inner.lock().await;
        guard.append_entries(entries);
        // In-memory storage is "durable" immediately.
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let mut guard = self.inner.lock().await;
        guard.truncate_since(log_id.index);
        Ok(())
    }

    async fn purge(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let mut guard = self.inner.lock().await;
        guard.purge_up_to(log_id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use openraft::LogId;

    use super::*;

    fn log_id(term: u64, index: u64) -> LogId<u64> {
        LogId::new(openraft::CommittedLeaderId::new(term, 0), index)
    }

    fn entry(term: u64, index: u64) -> Entry<TypeConfig> {
        Entry {
            log_id: log_id(term, index),
            payload: openraft::EntryPayload::Blank,
        }
    }

    #[tokio::test]
    async fn empty_log_state() {
        let mut store = MemLogStore::new();
        let state = store.get_log_state().await.unwrap();
        assert!(state.last_purged_log_id.is_none());
        assert!(state.last_log_id.is_none());
    }

    #[tokio::test]
    async fn append_entries_and_read_back() {
        let store = MemLogStore::new();
        let mut reader = MemLogReader {
            inner: Arc::clone(&store.inner),
        };

        // Append entries directly via the inner (since LogFlushed is
        // pub(crate) in openraft, we test the logic without the callback).
        {
            let mut guard = store.inner.lock().await;
            guard.append_entries(vec![entry(1, 1), entry(1, 2), entry(1, 3)]);
        }

        let entries = reader.try_get_log_entries(1..4).await.unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].log_id.index, 1);
        assert_eq!(entries[2].log_id.index, 3);
    }

    #[tokio::test]
    async fn append_updates_last_log_id() {
        let store = MemLogStore::new();
        {
            let mut guard = store.inner.lock().await;
            guard.append_entries(vec![entry(1, 1), entry(1, 2)]);
        }

        let guard = store.inner.lock().await;
        let state = guard.get_log_state();
        assert_eq!(state.last_log_id, Some(log_id(1, 2)));
        assert!(state.last_purged_log_id.is_none());
    }

    #[tokio::test]
    async fn truncate_removes_entries_from_index() {
        let store = MemLogStore::new();
        {
            let mut guard = store.inner.lock().await;
            guard.append_entries(vec![entry(1, 1), entry(1, 2), entry(1, 3), entry(1, 4)]);
            // Truncate since index 3 (inclusive).
            guard.truncate_since(3);
        }

        let mut reader = MemLogReader {
            inner: Arc::clone(&store.inner),
        };
        let entries = reader.try_get_log_entries(1..5).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].log_id.index, 1);
        assert_eq!(entries[1].log_id.index, 2);
    }

    #[tokio::test]
    async fn purge_removes_entries_up_to_index() {
        let store = MemLogStore::new();
        {
            let mut guard = store.inner.lock().await;
            guard.append_entries(vec![entry(1, 1), entry(1, 2), entry(1, 3), entry(1, 4)]);
            guard.purge_up_to(log_id(1, 2));
        }

        let guard = store.inner.lock().await;
        let state = guard.get_log_state();
        assert_eq!(state.last_purged_log_id, Some(log_id(1, 2)));
        assert_eq!(state.last_log_id, Some(log_id(1, 4)));
        drop(guard);

        let mut reader = MemLogReader {
            inner: Arc::clone(&store.inner),
        };
        let entries = reader.try_get_log_entries(1..5).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].log_id.index, 3);
    }

    #[tokio::test]
    async fn save_and_read_vote() {
        let mut store = MemLogStore::new();
        assert!(store.read_vote().await.unwrap().is_none());

        let vote = Vote::new(1, 42);
        store.save_vote(&vote).await.unwrap();
        let read = store.read_vote().await.unwrap().unwrap();
        assert_eq!(read, vote);
    }

    #[tokio::test]
    async fn log_reader_reads_range() {
        let store = MemLogStore::new();
        {
            let mut guard = store.inner.lock().await;
            guard.append_entries(vec![
                entry(1, 1),
                entry(1, 2),
                entry(1, 3),
                entry(1, 4),
                entry(1, 5),
            ]);
        }

        let mut reader = MemLogReader {
            inner: Arc::clone(&store.inner),
        };
        let entries = reader.try_get_log_entries(2..4).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].log_id.index, 2);
        assert_eq!(entries[1].log_id.index, 3);
    }

    #[tokio::test]
    async fn log_reader_empty_range_returns_empty() {
        let store = MemLogStore::new();
        {
            let mut guard = store.inner.lock().await;
            guard.append_entries(vec![entry(1, 1)]);
        }

        let mut reader = MemLogReader {
            inner: Arc::clone(&store.inner),
        };
        let entries = reader.try_get_log_entries(5..10).await.unwrap();
        assert!(entries.is_empty());
    }
}
