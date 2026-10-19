//! The task-array node's control fence: the newest control version it has
//! acted on, so a delayed sync from an older leader or snapshot is refused,
//! across restarts too.
//!
//! The leader's applied index moves on every tick that writes anything, so
//! under load the fence changes on nearly every sync. It lives in a
//! [`fence_file`](super::fence_file), rewritten in place with one flush.

use std::io;
use std::path::Path;

use super::fence_file::{self, Position};
use super::task_array_node::ControlVersion;

/// The fence's file name in the node's task directory.
pub(crate) const FILE_NAME: &str = "control.fence";

const MAGIC: &[u8; 4] = b"RBCF";

/// The fence as read from disk.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fence {
    /// The newest version the node has made durable.
    pub version: ControlVersion,
    position: Position,
}

/// Read the fence at `path`. `None` when there is no fence file yet; an
/// error when the file exists but holds no version, because reading that
/// as no fence would let any delayed sync through.
pub(crate) fn read(path: &Path) -> io::Result<Option<Fence>> {
    let Some(contents) = fence_file::read(path, MAGIC)? else {
        return Ok(None);
    };
    let word = |at: usize| {
        let bytes = contents.payload.get(at..at + 8)?;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    };
    match (contents.payload.len(), word(0), word(8), word(16)) {
        (24, Some(epoch), Some(term), Some(index)) => Ok(Some(Fence {
            version: ControlVersion { epoch, term, index },
            position: contents.position,
        })),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("control fence {} holds no version", path.display()),
        )),
    }
}

/// Make `version` the fence at `path`, durably, before the node acts on
/// it. Returns how many flushes that cost: one to move an existing fence,
/// two to create one (the file and its directory).
pub(crate) fn write(
    path: &Path,
    current: Option<&Fence>,
    version: ControlVersion,
) -> io::Result<u64> {
    let mut payload = [0u8; 24];
    payload[..8].copy_from_slice(&version.epoch.to_le_bytes());
    payload[8..16].copy_from_slice(&version.term.to_le_bytes());
    payload[16..].copy_from_slice(&version.index.to_le_bytes());
    let position = current.map(|fence| &fence.position);
    Ok(fence_file::write(path, MAGIC, position, &payload)?.flushes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(index: u64) -> ControlVersion {
        ControlVersion {
            epoch: 1,
            term: 2,
            index,
        }
    }

    #[test]
    fn a_fence_reads_back_the_version_last_written() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        assert!(read(&path).unwrap().is_none());
        assert_eq!(write(&path, None, version(7)).unwrap(), 2);
        let mut fence = read(&path).unwrap().unwrap();
        assert_eq!(fence.version, version(7));
        for index in 8..=10 {
            assert_eq!(write(&path, Some(&fence), version(index)).unwrap(), 1);
            fence = read(&path).unwrap().unwrap();
            assert_eq!(fence.version, version(index));
        }
    }

    #[test]
    fn a_fence_whose_payload_is_not_a_version_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        fence_file::write(&path, MAGIC, None, b"short").unwrap();
        assert_eq!(read(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
