//! A task array's grant fence on one node: the highest attempt the node has
//! accepted for each chunk, so a delayed sync can't make it run an older
//! attempt of a chunk it has already been granted again.
//!
//! It changes on every new grant, and under load the leader grants chunks
//! every tick, so it lives in a [`fence_file`](super::fence_file),
//! rewritten in place with one flush (#902).

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use super::fence_file::{self, Position, Written};

/// The fence's file name in each array's directory.
pub(crate) const FILE_NAME: &str = "grants.fence";

const MAGIC: &[u8; 4] = b"RBGF";
/// One grant: the chunk (`u32`) and its highest attempt (`u64`).
const GRANT_LEN: usize = 4 + 8;

/// Read the fence at `path`: the highest attempt per chunk, and where it
/// sits for the next [`write`]. `None` when there is no fence file yet; an
/// error when it exists but holds no valid set of grants.
pub(crate) fn read(path: &Path) -> io::Result<Option<(BTreeMap<u32, u64>, Position)>> {
    let Some(contents) = fence_file::read(path, MAGIC)? else {
        return Ok(None);
    };
    let entries = contents.payload.chunks(GRANT_LEN);
    let highest = entries
        .map(|grant| {
            let chunk = u32::from_le_bytes(grant.get(..4)?.try_into().ok()?);
            let attempt = u64::from_le_bytes(grant.get(4..)?.try_into().ok()?);
            Some((chunk, attempt))
        })
        .collect::<Option<BTreeMap<_, _>>>()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("grant fence {} holds a partial grant", path.display()),
            )
        })?;
    Ok(Some((highest, contents.position)))
}

/// Make `highest` the fence at `path`, durably, before the node acts on
/// it. `current` is where the previous one sits, if there is one.
pub(crate) fn write(
    path: &Path,
    current: Option<&Position>,
    highest: &BTreeMap<u32, u64>,
) -> io::Result<Written> {
    let mut payload = Vec::with_capacity(highest.len() * GRANT_LEN);
    for (chunk, attempt) in highest {
        payload.extend_from_slice(&chunk.to_le_bytes());
        payload.extend_from_slice(&attempt.to_le_bytes());
    }
    fence_file::write(path, MAGIC, current, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fence_reads_back_the_grants_last_written() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        assert!(read(&path).unwrap().is_none());
        let empty = BTreeMap::new();
        let mut position = write(&path, None, &empty).unwrap().position;
        assert_eq!(read(&path).unwrap().unwrap().0, empty);
        let mut highest = BTreeMap::new();
        for chunk in 0..100u32 {
            highest.insert(chunk, u64::from(chunk) << 33 | 1);
            position = write(&path, Some(&position), &highest).unwrap().position;
            let (read_back, at) = read(&path).unwrap().unwrap();
            assert_eq!(read_back, highest);
            assert_eq!(at, position);
        }
    }

    #[test]
    fn a_fence_whose_payload_is_not_whole_grants_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        fence_file::write(&path, MAGIC, None, &[0u8; 13]).unwrap();
        assert_eq!(read(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
