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
use crate::sesame::raft_encryption::{self, EncryptedEntry};

/// Log entries: index → `bincode(Entry)`.
const ENTRIES: TableDefinition<u64, &[u8]> = TableDefinition::new("raft_log_entries");
/// Small metadata: vote / committed / last-purged → bincode.
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_log_meta");

const VOTE_KEY: &str = "vote";
const COMMITTED_KEY: &str = "committed";
const PURGED_KEY: &str = "last_purged";

/// First byte of an encrypted log value.
///
/// Plaintext values are `serde_json::to_vec(&Entry)`, which always begins
/// with `{` (0x7B); this marker is a byte a JSON entry can never start with,
/// so a stored value is encrypted iff its first byte equals this marker. That
/// one-byte test is what lets a plaintext log written before encryption (or on
/// a keyless cluster) keep loading transparently after a key is introduced —
/// the backward/forward compatibility the durable log's replay path depends on
/// (an unreadable log must not be mistaken for a fresh one, CP3).
const ENCRYPTED_MARKER: u8 = 0x01;

/// Durable Raft log + vote storage backed by redb.
///
/// `Clone` shares the underlying `Arc<Database>`, so the log reader (used by
/// replication tasks) sees the same data as the main Raft task.
#[derive(Clone)]
pub struct DurableLogStore {
    db: Arc<Database>,
    /// Input key material for at-rest log encryption, or `None` for a keyless
    /// (dev / pre-security) cluster that stores entries as plaintext JSON.
    ///
    /// When present this is the cluster master key (`wrapping_ikm`); it is fed
    /// as HKDF IKM to [`raft_encryption::encrypt_entry`] /
    /// [`raft_encryption::decrypt_entry`], each of which calls
    /// `derive_log_encryption_key` internally with a fresh per-entry salt.
    key: Option<Vec<u8>>,
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
    /// Open (or create) a plaintext durable log store at `path`.
    ///
    /// Equivalent to [`open_with_key`](Self::open_with_key) with no key: entries
    /// are stored as plaintext JSON, exactly as before at-rest encryption
    /// existed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, redb::Error> {
        Self::open_with_key(path, None)
    }

    /// Open (or create) a durable log store at `path`, optionally encrypting
    /// entries at rest.
    ///
    /// When `key` is `Some`, each entry's value is encrypted (AES-256-GCM,
    /// per-entry salt+nonce) before being written and decrypted on read. When
    /// it is `None`, entries are stored as plaintext JSON. Either way a value
    /// written by an earlier run — plaintext or encrypted — still loads: the
    /// read path detects the on-disk format per value, so introducing (or, with
    /// the same key, retaining) encryption never breaks the existing
    /// open/replay path.
    pub fn open_with_key(
        path: impl AsRef<Path>,
        key: Option<Vec<u8>>,
    ) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        // Materialise the tables so reads on a fresh store don't error.
        let wtx = db.begin_write()?;
        {
            wtx.open_table(ENTRIES)?;
            wtx.open_table(META)?;
        }
        wtx.commit()?;
        Ok(Self {
            db: Arc::new(db),
            key,
        })
    }

    /// Encode an entry into the bytes stored in redb.
    ///
    /// Plaintext JSON when keyless; when a key is present, the JSON is
    /// encrypted and the `EncryptedEntry` is bincoded behind a one-byte marker
    /// so the read path can tell the two formats apart.
    fn encode_value(&self, entry: &Entry<TypeConfig>) -> Result<Vec<u8>, StorageError<u64>> {
        // JSON, not bincode: log entries carry `RaftRequest`, whose `AppSpec`
        // uses config types (`Replicas`, `ResourceRange`) with `deserialize_any`
        // for TOML ergonomics. bincode is not self-describing and cannot drive
        // `deserialize_any`, so it corrupts on read-back. The snapshot already
        // uses JSON for the same reason. Votes/log-ids stay bincode — they're
        // plain openraft numeric types.
        let json = serde_json::to_vec(entry).map_err(write_err)?;
        match &self.key {
            None => Ok(json),
            Some(ikm) => {
                let encrypted = raft_encryption::encrypt_entry(&json, ikm).map_err(write_err)?;
                // bincode is safe here: `EncryptedEntry` is plain byte
                // vectors/arrays, no `deserialize_any`.
                let body = bincode::serialize(&encrypted).map_err(write_err)?;
                let mut out = Vec::with_capacity(body.len() + 1);
                out.push(ENCRYPTED_MARKER);
                out.extend_from_slice(&body);
                Ok(out)
            }
        }
    }

    /// Decode an entry from the bytes stored in redb, transparently handling
    /// both plaintext and encrypted values.
    ///
    /// An encrypted value (marker byte) requires the key that wrote it; a
    /// plaintext value is decoded directly regardless of whether a key is now
    /// configured. Refusing to decode an encrypted value with no key available
    /// surfaces as a read error rather than silently losing log entries.
    fn decode_value(&self, bytes: &[u8]) -> Result<Entry<TypeConfig>, StorageError<u64>> {
        if bytes.first() == Some(&ENCRYPTED_MARKER) {
            let encrypted: EncryptedEntry = bincode::deserialize(&bytes[1..]).map_err(read_err)?;
            let ikm = self.key.as_ref().ok_or_else(|| {
                read_err(std::io::Error::other(
                    "encrypted raft log entry but no encryption key available",
                ))
            })?;
            let json = raft_encryption::decrypt_entry(&encrypted, ikm).map_err(read_err)?;
            decode_entry(&json)
        } else {
            decode_entry(bytes)
        }
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
            Some(v) => Ok(Some(self.decode_value(v.value())?)),
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
            out.push(self.decode_value(v.value())?);
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
                let bytes = self.encode_value(&entry)?;
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

    /// Stand-in for the cluster master key (`wrapping_ikm`). HKDF accepts any
    /// IKM length, so a plain byte string is fine.
    fn test_key() -> Vec<u8> {
        b"cluster-master-key-for-raft-log-tests".to_vec()
    }

    /// Read the exact bytes stored under `index`, to assert the on-disk format.
    fn raw_entry_bytes(store: &DurableLogStore, index: u64) -> Vec<u8> {
        let rtx = store.db.begin_read().unwrap();
        let t = rtx.open_table(ENTRIES).unwrap();
        t.get(index).unwrap().unwrap().value().to_vec()
    }

    #[tokio::test]
    async fn encrypted_log_round_trips_append_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.redb");
        let key = test_key();

        let mut store = DurableLogStore::open_with_key(&path, Some(key.clone())).unwrap();
        store.write_entries(vec![entry(1, 1), entry(2, 2)]).unwrap();

        // On disk it is ciphertext behind the marker, not the plaintext JSON.
        let raw = raw_entry_bytes(&store, 1);
        assert_eq!(raw.first(), Some(&ENCRYPTED_MARKER));
        let plaintext = serde_json::to_vec(&entry(1, 1)).unwrap();
        assert!(
            raft_encryption::is_encrypted(&raw, &plaintext),
            "stored value must not contain the plaintext entry"
        );

        let mut reader = store.get_log_reader().await;
        let entries = reader.try_get_log_entries(1..3).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].log_id.index, 1);
        assert_eq!(entries[1].log_id.index, 2);

        // Survives a reopen with the same key. The reader holds a clone of the
        // db Arc, so it must be dropped too before redb releases the file.
        drop(reader);
        drop(store);
        let mut store = DurableLogStore::open_with_key(&path, Some(key)).unwrap();
        let mut reader = store.get_log_reader().await;
        assert_eq!(reader.try_get_log_entries(1..3).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn plaintext_log_round_trips_without_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.redb");

        let mut store = DurableLogStore::open_with_key(&path, None).unwrap();
        store.write_entries(vec![entry(1, 1)]).unwrap();

        // Keyless: the value is plaintext JSON (starts with `{`), never the marker.
        let raw = raw_entry_bytes(&store, 1);
        assert_eq!(raw.first(), Some(&b'{'));

        let mut reader = store.get_log_reader().await;
        assert_eq!(reader.try_get_log_entries(1..2).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn key_store_reads_previously_plaintext_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.redb");

        // First run: keyless, so the entry is written as plaintext.
        {
            let store = DurableLogStore::open(&path).unwrap();
            store.write_entries(vec![entry(1, 1)]).unwrap();
        }

        // Second run: the SAME store reopened with a key. The pre-existing
        // plaintext entry must still load, and new entries are encrypted.
        let mut store = DurableLogStore::open_with_key(&path, Some(test_key())).unwrap();
        store.write_entries(vec![entry(2, 2)]).unwrap();

        assert_eq!(raw_entry_bytes(&store, 1).first(), Some(&b'{'));
        assert_eq!(raw_entry_bytes(&store, 2).first(), Some(&ENCRYPTED_MARKER));

        let mut reader = store.get_log_reader().await;
        let entries = reader.try_get_log_entries(1..3).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].log_id.index, 1);
        assert_eq!(entries[1].log_id.index, 2);
    }

    #[tokio::test]
    async fn encrypted_entry_without_key_errors_rather_than_vanishing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.redb");

        {
            let store = DurableLogStore::open_with_key(&path, Some(test_key())).unwrap();
            store.write_entries(vec![entry(1, 1)]).unwrap();
        }

        // Reopen with no key: the encrypted entry cannot be decoded. It must
        // surface as an error, never as a silently empty log — dropping
        // committed entries is the exact split-brain hazard the durable log
        // guards against (CP3).
        let mut store = DurableLogStore::open(&path).unwrap();
        let mut reader = store.get_log_reader().await;
        assert!(reader.try_get_log_entries(1..2).await.is_err());
    }
}
