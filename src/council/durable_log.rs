//! redb-backed durable Raft log storage.
//!
//! Persists the vote, committed pointer, and log entries to a crash-safe
//! embedded key-value store so a restarted node remembers what it voted for and
//! what it logged — the prerequisite for Raft's safety guarantees (and the fix
//! for the split-brain-on-restart bug, together with the bootstrap guard in
//! `cluster::runtime`). Mirrors [`super::log_store::MemLogStore`]'s trait
//! surface; the in-memory store is kept for tests.
//!
//! openraft's `StorageError` is large (~224 bytes) and dictated by its trait
//! API, so `result_large_err` on the private helpers that propagate it is
//! unavoidable — allow it module-wide.
#![allow(clippy::result_large_err)]

use std::fmt::Debug;
use std::ops::{Bound, RangeBounds};
use std::path::Path;
use std::sync::Arc;

use openraft::storage::{LogFlushed, LogState, RaftLogStorage};
use openraft::{Entry, LogId, RaftLogReader, StorageError, StorageIOError, Vote};
use redb::{Database, ReadableTable, TableDefinition};

use super::types::TypeConfig;

/// Log entries: index → `bincode(Entry)`.
const ENTRIES: TableDefinition<u64, &[u8]> = TableDefinition::new("raft_log_entries");
/// Small metadata: vote / committed / last-purged → bincode.
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_log_meta");

const VOTE_KEY: &str = "vote";
const COMMITTED_KEY: &str = "committed";
const PURGED_KEY: &str = "last_purged";

/// Durable Raft log + vote storage backed by redb.
///
/// `Clone` shares the underlying `Arc<Database>`, so the log reader (used by
/// replication tasks) sees the same data as the main Raft task.
#[derive(Clone)]
pub struct DurableLogStore {
    db: Arc<Database>,
}

/// Map any error into an openraft read-logs storage error.
fn read_err<E: std::error::Error + 'static>(e: E) -> StorageError<u64> {
    StorageError::from(StorageIOError::read_logs(&e))
}

/// Map any error into an openraft write-logs storage error.
fn write_err<E: std::error::Error + 'static>(e: E) -> StorageError<u64> {
    StorageError::from(StorageIOError::write_logs(&e))
}

/// Decode a log entry from stored bytes.
///
/// Reads JSON (the current format); silently accepting the old bincode
/// format is deliberately not attempted — the entry payloads it could
/// hold (`AppSpec`) never round-tripped through bincode anyway.
fn decode_entry(bytes: &[u8]) -> Result<Entry<TypeConfig>, StorageError<u64>> {
    serde_json::from_slice(bytes).map_err(read_err)
}

impl DurableLogStore {
    /// Open (or create) a durable log store at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        // Materialise the tables so reads on a fresh store don't error.
        let wtx = db.begin_write()?;
        {
            wtx.open_table(ENTRIES)?;
            wtx.open_table(META)?;
        }
        wtx.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Whether the store has never been written to (no vote and no log).
    ///
    /// The bootstrap guard uses this: only a genuinely fresh node initialises a
    /// new cluster; a restarted node has a populated store and must resume.
    /// Read errors propagate: a store we cannot read is unknown, not fresh —
    /// re-bootstrapping on top of a damaged store is the exact split-brain
    /// the durable log exists to prevent (C3/CP3).
    pub fn is_fresh(&self) -> Result<bool, StorageError<u64>> {
        Ok(self.get_vote()?.is_none() && self.last_log_index()?.is_none())
    }

    /// The last log id compacted away, `None` if nothing was ever purged.
    ///
    /// Startup validation compares this against the loaded snapshot's
    /// `last_applied`: a purge past what the snapshot covers means the state
    /// cannot be reconstructed.
    pub fn last_purged_log_id(&self) -> Result<Option<LogId<u64>>, StorageError<u64>> {
        self.last_purged()
    }

    // -- redb helpers ------------------------------------------------------

    fn put_meta(&self, key: &str, value: &[u8]) -> Result<(), StorageError<u64>> {
        let wtx = self.db.begin_write().map_err(write_err)?;
        {
            let mut t = wtx.open_table(META).map_err(write_err)?;
            t.insert(key, value).map_err(write_err)?;
        }
        wtx.commit().map_err(write_err)?;
        Ok(())
    }

    fn del_meta(&self, key: &str) -> Result<(), StorageError<u64>> {
        let wtx = self.db.begin_write().map_err(write_err)?;
        {
            let mut t = wtx.open_table(META).map_err(write_err)?;
            t.remove(key).map_err(write_err)?;
        }
        wtx.commit().map_err(write_err)?;
        Ok(())
    }

    fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError<u64>> {
        let rtx = self.db.begin_read().map_err(read_err)?;
        let t = rtx.open_table(META).map_err(read_err)?;
        Ok(t.get(key).map_err(read_err)?.map(|v| v.value().to_vec()))
    }

    fn get_vote(&self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
        match self.get_meta(VOTE_KEY)? {
            Some(bytes) => Ok(Some(bincode::deserialize(&bytes).map_err(read_err)?)),
            None => Ok(None),
        }
    }

    fn last_log_index(&self) -> Result<Option<u64>, StorageError<u64>> {
        let rtx = self.db.begin_read().map_err(read_err)?;
        let t = rtx.open_table(ENTRIES).map_err(read_err)?;
        Ok(t.last().map_err(read_err)?.map(|(k, _)| k.value()))
    }

    fn read_entry(&self, index: u64) -> Result<Option<Entry<TypeConfig>>, StorageError<u64>> {
        let rtx = self.db.begin_read().map_err(read_err)?;
        let t = rtx.open_table(ENTRIES).map_err(read_err)?;
        match t.get(index).map_err(read_err)? {
            Some(v) => Ok(Some(decode_entry(v.value())?)),
            None => Ok(None),
        }
    }

    fn read_range<RB: RangeBounds<u64>>(
        &self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<u64>> {
        let bounds = (
            copy_bound(range.start_bound()),
            copy_bound(range.end_bound()),
        );
        let rtx = self.db.begin_read().map_err(read_err)?;
        let t = rtx.open_table(ENTRIES).map_err(read_err)?;
        let mut out = Vec::new();
        for row in t.range::<u64>(bounds).map_err(read_err)? {
            let (_, v) = row.map_err(read_err)?;
            out.push(decode_entry(v.value())?);
        }
        Ok(out)
    }

    /// Directly persist entries. Shared by `append` and used by tests (openraft's
    /// `LogFlushed` callback is `pub(crate)`, so tests exercise this instead).
    fn write_entries(
        &self,
        entries: impl IntoIterator<Item = Entry<TypeConfig>>,
    ) -> Result<(), StorageError<u64>> {
        let wtx = self.db.begin_write().map_err(write_err)?;
        {
            let mut t = wtx.open_table(ENTRIES).map_err(write_err)?;
            for entry in entries {
                // JSON, not bincode: log entries carry `RaftRequest`, whose
                // `AppSpec` uses config types (`Replicas`, `ResourceRange`)
                // with `deserialize_any` for TOML ergonomics. bincode is not
                // self-describing and cannot drive `deserialize_any`, so it
                // corrupts on read-back. The snapshot already uses JSON for
                // the same reason. Votes/log-ids stay bincode — they're
                // plain openraft numeric types.
                let bytes = serde_json::to_vec(&entry).map_err(write_err)?;
                t.insert(entry.log_id.index, bytes.as_slice())
                    .map_err(write_err)?;
            }
        }
        // commit() fsyncs — the entries are durable once this returns.
        wtx.commit().map_err(write_err)?;
        Ok(())
    }

    fn last_purged(&self) -> Result<Option<LogId<u64>>, StorageError<u64>> {
        match self.get_meta(PURGED_KEY)? {
            Some(bytes) => Ok(Some(bincode::deserialize(&bytes).map_err(read_err)?)),
            None => Ok(None),
        }
    }
}

/// Copy a `Bound<&u64>` into an owned `Bound<u64>`.
fn copy_bound(b: Bound<&u64>) -> Bound<u64> {
    match b {
        Bound::Included(&i) => Bound::Included(i),
        Bound::Excluded(&i) => Bound::Excluded(i),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// Read-only handle into the durable log store.
#[derive(Clone)]
pub struct DurableLogReader {
    store: DurableLogStore,
}

impl RaftLogReader<TypeConfig> for DurableLogReader {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<u64>> {
        self.store.read_range(range)
    }
}

impl RaftLogReader<TypeConfig> for DurableLogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<u64>> {
        self.read_range(range)
    }
}

impl RaftLogStorage<TypeConfig> for DurableLogStore {
    type LogReader = DurableLogReader;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<u64>> {
        let last_purged = self.last_purged()?;
        let last_log_id = match self.last_log_index()? {
            Some(index) => self.read_entry(index)?.map(|e| e.log_id).or(last_purged),
            None => last_purged,
        };
        Ok(LogState {
            last_purged_log_id: last_purged,
            last_log_id,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        DurableLogReader {
            store: self.clone(),
        }
    }

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<u64>> {
        let bytes = bincode::serialize(vote).map_err(write_err)?;
        self.put_meta(VOTE_KEY, &bytes)
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
        self.get_vote()
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<u64>>,
    ) -> Result<(), StorageError<u64>> {
        match committed {
            Some(log_id) => {
                let bytes = bincode::serialize(&log_id).map_err(write_err)?;
                self.put_meta(COMMITTED_KEY, &bytes)
            }
            None => self.del_meta(COMMITTED_KEY),
        }
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<u64>>, StorageError<u64>> {
        match self.get_meta(COMMITTED_KEY)? {
            Some(bytes) => Ok(Some(bincode::deserialize(&bytes).map_err(read_err)?)),
            None => Ok(None),
        }
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
        // Entries are on disk and fsynced once write_entries returns; only then
        // do we signal openraft that the write completed.
        self.write_entries(entries)?;
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let wtx = self.db.begin_write().map_err(write_err)?;
        {
            let mut t = wtx.open_table(ENTRIES).map_err(write_err)?;
            // Remove everything from log_id.index onward. A row read error
            // propagates: silently skipping a key would leave a stale entry
            // that a later election could resurrect.
            let mut keys = Vec::new();
            for row in t.range::<u64>(log_id.index..).map_err(write_err)? {
                let (k, _) = row.map_err(write_err)?;
                keys.push(k.value());
            }
            for k in keys {
                t.remove(k).map_err(write_err)?;
            }
        }
        wtx.commit().map_err(write_err)?;
        Ok(())
    }

    async fn purge(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let bytes = bincode::serialize(&log_id).map_err(write_err)?;
        let wtx = self.db.begin_write().map_err(write_err)?;
        {
            let mut meta = wtx.open_table(META).map_err(write_err)?;
            meta.insert(PURGED_KEY, bytes.as_slice())
                .map_err(write_err)?;
            let mut t = wtx.open_table(ENTRIES).map_err(write_err)?;
            // Same as truncate: propagate row errors instead of skipping keys.
            let mut keys = Vec::new();
            for row in t.range::<u64>(..=log_id.index).map_err(write_err)? {
                let (k, _) = row.map_err(write_err)?;
                keys.push(k.value());
            }
            for k in keys {
                t.remove(k).map_err(write_err)?;
            }
        }
        wtx.commit().map_err(write_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use openraft::{CommittedLeaderId, EntryPayload, LogId};

    use super::*;

    fn log_id(term: u64, index: u64) -> LogId<u64> {
        LogId::new(CommittedLeaderId::new(term, 0), index)
    }

    fn entry(term: u64, index: u64) -> Entry<TypeConfig> {
        Entry {
            log_id: log_id(term, index),
            payload: EntryPayload::Blank,
        }
    }

    #[tokio::test]
    async fn durable_log_round_trips_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.redb");

        // First "run": write vote, entries, committed.
        {
            let mut store = DurableLogStore::open(&path).unwrap();
            assert!(store.is_fresh().unwrap());

            store.save_vote(&Vote::new(3, 42)).await.unwrap();
            store
                .write_entries(vec![entry(1, 1), entry(1, 2), entry(2, 3)])
                .unwrap();
            store.save_committed(Some(log_id(2, 3))).await.unwrap();

            assert!(!store.is_fresh().unwrap());
        }

        // Second "run": reopen the same path; everything must have survived.
        let mut store = DurableLogStore::open(&path).unwrap();
        assert_eq!(store.read_vote().await.unwrap(), Some(Vote::new(3, 42)));
        assert_eq!(store.read_committed().await.unwrap(), Some(log_id(2, 3)));

        let state = store.get_log_state().await.unwrap();
        assert_eq!(state.last_log_id, Some(log_id(2, 3)));
        assert!(state.last_purged_log_id.is_none());

        let mut reader = store.get_log_reader().await;
        let entries = reader.try_get_log_entries(1..4).await.unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].log_id.index, 1);
        assert_eq!(entries[2].log_id.index, 3);
    }

    #[test]
    fn is_fresh_errors_when_the_vote_is_unreadable() {
        // A vote that exists but cannot be decoded must NOT read as "fresh".
        // Treating an unreadable store as fresh would re-bootstrap a new
        // single-node cluster on top of a damaged one — the exact
        // split-brain the durable store exists to prevent (C3/CP3).
        let dir = tempfile::tempdir().unwrap();
        let store = DurableLogStore::open(dir.path().join("store.redb")).unwrap();
        store.put_meta(VOTE_KEY, &[]).unwrap();
        assert!(
            store.is_fresh().is_err(),
            "an unreadable vote must propagate as an error, not read as fresh"
        );
    }

    #[tokio::test]
    async fn last_purged_log_id_reports_the_purge_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = DurableLogStore::open(dir.path().join("store.redb")).unwrap();
        assert_eq!(store.last_purged_log_id().unwrap(), None);

        store.write_entries(vec![entry(1, 1), entry(1, 2)]).unwrap();
        store.purge(log_id(1, 2)).await.unwrap();
        assert_eq!(store.last_purged_log_id().unwrap(), Some(log_id(1, 2)));
    }

    #[tokio::test]
    async fn truncate_and_purge_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.redb");
        let mut store = DurableLogStore::open(&path).unwrap();

        store
            .write_entries(vec![entry(1, 1), entry(1, 2), entry(1, 3), entry(1, 4)])
            .unwrap();

        // Truncate from index 3 onward.
        store.truncate(log_id(1, 3)).await.unwrap();
        let mut reader = store.get_log_reader().await;
        assert_eq!(reader.try_get_log_entries(1..10).await.unwrap().len(), 2);

        // Purge up to index 1 (inclusive).
        store.purge(log_id(1, 1)).await.unwrap();
        let state = store.get_log_state().await.unwrap();
        assert_eq!(state.last_purged_log_id, Some(log_id(1, 1)));
        let remaining = store.get_log_reader().await;
        assert_eq!(
            remaining.clone().try_get_log_entries(0..10).await.unwrap()[0]
                .log_id
                .index,
            2
        );
    }
}
