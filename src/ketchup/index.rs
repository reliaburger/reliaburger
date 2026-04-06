//! Sparse timestamp index for log files.
//!
//! Writes `(offset, timestamp)` pairs at regular byte boundaries
//! (every ~4KB of log data). Enables binary search for time-range
//! queries without scanning the entire log file.

use std::io::{self, Write};
use std::path::Path;

/// Size of each index entry: u64 offset + u64 timestamp = 16 bytes.
const ENTRY_SIZE: usize = 16;

/// Byte interval between index entries.
pub const INDEX_INTERVAL: u64 = 4096;

/// A single index entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    /// Byte offset in the log file.
    pub offset: u64,
    /// Timestamp (seconds since epoch) at this offset.
    pub timestamp: u64,
}

/// Sparse timestamp index.
pub struct SparseIndex {
    entries: Vec<IndexEntry>,
}

impl SparseIndex {
    /// Create an empty index.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Add an entry.
    pub fn add(&mut self, offset: u64, timestamp: u64) {
        self.entries.push(IndexEntry { offset, timestamp });
    }

    /// Find the byte offset for the first entry at or after `timestamp`.
    /// Returns 0 if no entry is found (scan from start).
    pub fn find_offset(&self, timestamp: u64) -> u64 {
        match self
            .entries
            .binary_search_by_key(&timestamp, |e| e.timestamp)
        {
            Ok(i) => self.entries[i].offset,
            Err(i) => {
                if i > 0 {
                    self.entries[i - 1].offset
                } else {
                    0
                }
            }
        }
    }

    /// Number of entries in the index.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Write the index to a file.
    pub fn write_to(&self, path: &Path) -> io::Result<()> {
        let mut file = std::fs::File::create(path)?;
        for entry in &self.entries {
            file.write_all(&entry.offset.to_le_bytes())?;
            file.write_all(&entry.timestamp.to_le_bytes())?;
        }
        file.flush()?;
        Ok(())
    }

    /// Read an index from a file.
    pub fn read_from(path: &Path) -> io::Result<Self> {
        let data = std::fs::read(path)?;
        let mut entries = Vec::new();
        let mut pos = 0;
        while pos + ENTRY_SIZE <= data.len() {
            let offset = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
            let timestamp = u64::from_le_bytes(data[pos + 8..pos + 16].try_into().unwrap());
            entries.push(IndexEntry { offset, timestamp });
            pos += ENTRY_SIZE;
        }
        Ok(Self { entries })
    }
}

impl Default for SparseIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_index_find_returns_zero() {
        let index = SparseIndex::new();
        assert_eq!(index.find_offset(1000), 0);
    }

    #[test]
    fn find_exact_timestamp() {
        let mut index = SparseIndex::new();
        index.add(0, 100);
        index.add(4096, 200);
        index.add(8192, 300);

        assert_eq!(index.find_offset(200), 4096);
    }

    #[test]
    fn find_between_timestamps() {
        let mut index = SparseIndex::new();
        index.add(0, 100);
        index.add(4096, 200);
        index.add(8192, 300);

        // 150 is between 100 and 200 → return offset for 100
        assert_eq!(index.find_offset(150), 0);
        // 250 is between 200 and 300 → return offset for 200
        assert_eq!(index.find_offset(250), 4096);
    }

    #[test]
    fn find_before_all_entries() {
        let mut index = SparseIndex::new();
        index.add(4096, 200);

        assert_eq!(index.find_offset(100), 0);
    }

    #[test]
    fn write_and_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.idx");

        let mut index = SparseIndex::new();
        index.add(0, 100);
        index.add(4096, 200);
        index.add(8192, 300);
        index.write_to(&path).unwrap();

        let loaded = SparseIndex::read_from(&path).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded.find_offset(200), 4096);
    }

    #[test]
    fn index_len_and_is_empty() {
        let mut index = SparseIndex::new();
        assert!(index.is_empty());
        assert_eq!(index.len(), 0);

        index.add(0, 100);
        assert!(!index.is_empty());
        assert_eq!(index.len(), 1);
    }
}
