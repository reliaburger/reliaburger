//! The kept output of failed tasks: one append-only segment per chunk
//! grant, holding at most [`KEPT_FAILURES_PER_CHUNK`] tasks' output.
//!
//! A chunk with a bad command used to write one fsynced file per failed
//! task, so a million-task array that failed everywhere left a million
//! files behind. Now each chunk grant gets one file, named for the
//! chunk's index range and grant (`<first>-<last>-<grant>.seg`), and a
//! whole batch of outputs is appended with a single fsync. Past the
//! first few failures the output isn't worth the disk: the ledger still
//! records every failure's exit code and attempts, and `relish batch
//! logs` says plainly that the output wasn't kept.
//!
//! On-disk layout (little-endian), entries back to back:
//!
//! ```text
//! entry = index: u32, length: u32, bytes * length
//! ```
//!
//! A crash can tear the last entry. The ledger record is appended only
//! after its output, so a torn entry belongs to a task that will run
//! again; the next append cuts the torn tail off first.

use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::RangeInclusive;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Failed tasks per chunk whose output a node keeps. Later failures keep
/// their ledger record but not their output.
pub const KEPT_FAILURES_PER_CHUNK: usize = 16;

const ENTRY_HEADER_BYTES: usize = 8;

/// What one append did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Appended {
    /// Outputs written.
    pub kept: usize,
    /// Outputs dropped because the chunk already kept its share.
    pub dropped: usize,
    /// Whether the segment was fsynced (only when something was written).
    pub synced: bool,
}

fn segment_path(directory: &Path, range: &RangeInclusive<u32>, grant: u64) -> PathBuf {
    directory.join(format!("{}-{}-{grant}.seg", range.start(), range.end()))
}

/// Parse a segment name back into its range and grant.
fn parse_name(name: &str) -> Option<(RangeInclusive<u32>, u64)> {
    let stem = name.strip_suffix(".seg")?;
    let mut parts = stem.splitn(3, '-');
    let first = parts.next()?.parse().ok()?;
    let last = parts.next()?.parse().ok()?;
    let grant = parts.next()?.parse().ok()?;
    Some((first..=last, grant))
}

/// Walk a segment's entries, stopping at a torn tail. Returns the length
/// through the last complete entry.
fn scan(bytes: &[u8], mut entry: impl FnMut(u32, &[u8])) -> usize {
    let mut at = 0;
    while let Some(header) = bytes.get(at..at + ENTRY_HEADER_BYTES) {
        let index = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        let start = at + ENTRY_HEADER_BYTES;
        let Some(body) = bytes.get(start..start + length) else {
            break;
        };
        entry(index, body);
        at = start + length;
    }
    at
}

/// Append failed tasks' output to their chunk grant's segment, keeping at
/// most [`KEPT_FAILURES_PER_CHUNK`] distinct tasks, with one fsync for the
/// whole batch. Blocking I/O.
pub fn append(
    directory: &Path,
    range: &RangeInclusive<u32>,
    grant: u64,
    outputs: &[(u32, Vec<u8>)],
) -> std::io::Result<Appended> {
    let path = segment_path(directory, range, grant);
    let created = !path.exists();
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)?;
    let mut existing = Vec::new();
    file.read_to_end(&mut existing)?;
    let mut kept = BTreeSet::new();
    let valid = scan(&existing, |index, _| {
        kept.insert(index);
    });
    if valid < existing.len() {
        file.set_len(valid as u64)?;
    }
    file.seek(SeekFrom::Start(valid as u64))?;

    let mut appended = Appended::default();
    let mut batch = Vec::new();
    for (index, bytes) in outputs {
        if !kept.contains(index) && kept.len() >= KEPT_FAILURES_PER_CHUNK {
            appended.dropped += 1;
            continue;
        }
        let length = u32::try_from(bytes.len()).map_err(std::io::Error::other)?;
        kept.insert(*index);
        batch.extend_from_slice(&index.to_le_bytes());
        batch.extend_from_slice(&length.to_le_bytes());
        batch.extend_from_slice(bytes);
        appended.kept += 1;
    }
    if appended.kept > 0 {
        file.write_all(&batch)?;
        file.sync_data()?;
        appended.synced = true;
    }
    if created {
        std::fs::File::open(directory)?.sync_all()?;
    }
    Ok(appended)
}

/// The kept output of `index` at `grant`, if its chunk's segment holds
/// it. A task re-run under the same grant after a crash appears twice;
/// the later entry wins. Blocking I/O.
pub fn read(directory: &Path, index: u32, grant: u64) -> std::io::Result<Option<Vec<u8>>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let Some((range, segment_grant)) = entry.file_name().to_str().and_then(parse_name) else {
            continue;
        };
        if segment_grant != grant || !range.contains(&index) {
            continue;
        }
        let bytes = std::fs::read(entry.path())?;
        let mut found = None;
        scan(&bytes, |entry_index, body| {
            if entry_index == index {
                found = Some(body.to_vec());
            }
        });
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_torn_entry_is_cut_off_before_the_next_append() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &(0..=9), 1, &[(1, b"one".to_vec())]).unwrap();
        let path = segment_path(dir.path(), &(0..=9), 1);
        let mut torn = std::fs::read(&path).unwrap();
        torn.extend_from_slice(&[2, 0, 0, 0, 200, 0, 0, 0, b'x']);
        std::fs::write(&path, &torn).unwrap();

        append(dir.path(), &(0..=9), 1, &[(3, b"three".to_vec())]).unwrap();
        assert_eq!(read(dir.path(), 1, 1).unwrap(), Some(b"one".to_vec()));
        assert_eq!(read(dir.path(), 3, 1).unwrap(), Some(b"three".to_vec()));
        assert_eq!(read(dir.path(), 2, 1).unwrap(), None);
    }

    #[test]
    fn output_is_read_only_from_the_matching_grant() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &(10..=19), 1, &[(12, b"old".to_vec())]).unwrap();
        append(dir.path(), &(10..=19), 2, &[(12, b"new".to_vec())]).unwrap();
        assert_eq!(read(dir.path(), 12, 1).unwrap(), Some(b"old".to_vec()));
        assert_eq!(read(dir.path(), 12, 2).unwrap(), Some(b"new".to_vec()));
        assert_eq!(read(dir.path(), 12, 3).unwrap(), None);
        assert_eq!(read(&dir.path().join("missing"), 12, 1).unwrap(), None);
    }

    #[test]
    fn a_rerun_task_does_not_use_up_another_place() {
        let dir = tempfile::tempdir().unwrap();
        let first: Vec<_> = (0..KEPT_FAILURES_PER_CHUNK as u32)
            .map(|index| (index, b"first".to_vec()))
            .collect();
        append(dir.path(), &(0..=99), 1, &first).unwrap();
        let again = append(dir.path(), &(0..=99), 1, &[(0, b"again".to_vec())]).unwrap();
        assert_eq!(again.kept, 1);
        assert_eq!(read(dir.path(), 0, 1).unwrap(), Some(b"again".to_vec()));
    }
}
