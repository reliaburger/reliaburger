//! The task-array node's control fence: the newest control version it has
//! acted on, so a delayed sync from an older leader or snapshot is refused,
//! across restarts too.
//!
//! The leader's applied index moves on every tick that writes anything, so
//! under load the fence changes on nearly every sync. Replacing a file
//! (write a temporary, flush it, rename, flush the directory) costs two
//! flushes plus a create and a rename, and on macOS each of those queues
//! behind every other process's `F_FULLFSYNC` (#745). So the file has two
//! fixed slots, each a version with a CRC32, and an advance overwrites the
//! slot that doesn't hold the current version, in place, with one flush.
//! The other slot keeps the last version the node made durable, so a crash
//! that tears the write loses only the version it was writing, which the
//! node never acted on.

use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use super::task_array_node::ControlVersion;

/// The fence's file name in the node's task directory.
pub(crate) const FILE_NAME: &str = "control.fence";

const MAGIC: &[u8; 4] = b"RBCF";
/// Magic, epoch, term, index and a CRC32 of the rest.
const SLOT_LEN: usize = 4 + 3 * 8 + 4;
/// Slots sit in separate 512-byte sectors, so a torn write of one can't
/// reach the other, and the file stays small (a test bounds what a node's
/// task directory keeps after its runs retire).
const SLOT_STRIDE: u64 = 512;
const FILE_LEN: usize = SLOT_STRIDE as usize + SLOT_LEN;

/// The fence as read from disk.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fence {
    /// The newest version the node has made durable.
    pub version: ControlVersion,
    /// The slot holding it, 0 or 1.
    slot: u64,
}

fn encode(version: ControlVersion) -> [u8; SLOT_LEN] {
    let mut slot = [0u8; SLOT_LEN];
    slot[..4].copy_from_slice(MAGIC);
    slot[4..12].copy_from_slice(&version.epoch.to_le_bytes());
    slot[12..20].copy_from_slice(&version.term.to_le_bytes());
    slot[20..28].copy_from_slice(&version.index.to_le_bytes());
    let crc = crc32fast::hash(&slot[..28]);
    slot[28..].copy_from_slice(&crc.to_le_bytes());
    slot
}

/// The version in one slot, or `None` if the slot is torn or never written.
fn decode(slot: &[u8]) -> Option<ControlVersion> {
    let word = |at: usize| Some(u64::from_le_bytes(slot.get(at..at + 8)?.try_into().ok()?));
    let crc = u32::from_le_bytes(slot.get(28..32)?.try_into().ok()?);
    if slot.get(..4)? != MAGIC || crc32fast::hash(slot.get(..28)?) != crc {
        return None;
    }
    Some(ControlVersion {
        epoch: word(4)?,
        term: word(12)?,
        index: word(20)?,
    })
}

/// Read the fence at `path`. `None` when there is no fence file yet; an
/// error when the file exists but neither slot holds a version, because
/// reading that as no fence would let any delayed sync through.
pub(crate) fn read(path: &Path) -> io::Result<Option<Fence>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if bytes.len() != FILE_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("control fence is {} bytes, not {FILE_LEN}", bytes.len()),
        ));
    }
    let stride = SLOT_STRIDE as usize;
    let first = decode(&bytes[..SLOT_LEN]).map(|version| Fence { version, slot: 0 });
    let second =
        decode(&bytes[stride..stride + SLOT_LEN]).map(|version| Fence { version, slot: 1 });
    match (first, second) {
        (Some(a), Some(b)) => Ok(Some(if b.version > a.version { b } else { a })),
        (Some(only), None) | (None, Some(only)) => Ok(Some(only)),
        (None, None) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control fence has no valid slot",
        )),
    }
}

/// Create a fence holding `version` in both slots. The file appears whole
/// or not at all: it is written beside its final name, flushed, renamed
/// into place, and then its directory is flushed. Two flushes, once per
/// fence file.
pub(crate) fn create(path: &Path, version: ControlVersion) -> io::Result<()> {
    let mut bytes = vec![0u8; FILE_LEN];
    let slot = encode(version);
    let stride = SLOT_STRIDE as usize;
    bytes[..SLOT_LEN].copy_from_slice(&slot);
    bytes[stride..].copy_from_slice(&slot);
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("fence file has no parent directory"))?;
    std::fs::File::open(parent)?.sync_all()
}

/// Advance `current` to `version`, durably, with one flush: overwrite the
/// slot that doesn't hold `current`, in place.
pub(crate) fn advance(path: &Path, current: &Fence, version: ControlVersion) -> io::Result<()> {
    // O_NONBLOCK keeps an open of something that isn't a regular file (a
    // FIFO swapped in underneath) from waiting for ever; it changes nothing
    // for a regular file.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != FILE_LEN as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control fence changed underneath the node",
        ));
    }
    file.seek(SeekFrom::Start((1 - current.slot) * SLOT_STRIDE))?;
    file.write_all(&encode(version))?;
    // The file's size and directory entry are unchanged and already
    // durable, so its data is all that needs flushing. `sync_data` is
    // `fdatasync` on Linux and `F_FULLFSYNC` on macOS.
    file.sync_data()
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
    fn a_new_fence_reads_back_its_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        assert!(read(&path).unwrap().is_none());
        create(&path, version(7)).unwrap();
        assert_eq!(read(&path).unwrap().unwrap().version, version(7));
    }

    #[test]
    fn advancing_alternates_slots_so_the_previous_version_survives() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        create(&path, version(1)).unwrap();
        for index in 2..=5 {
            let current = read(&path).unwrap().unwrap();
            advance(&path, &current, version(index)).unwrap();
            let next = read(&path).unwrap().unwrap();
            assert_eq!(next.version, version(index));
            assert_ne!(next.slot, current.slot);
        }
    }

    #[test]
    fn a_fence_of_the_wrong_length_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        std::fs::write(&path, b"{}").unwrap();
        assert_eq!(read(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn advancing_a_fence_swapped_for_a_fifo_fails_instead_of_blocking() {
        // A test hangs a worker by swapping its fence for a FIFO with no
        // writer. A sync that read the real fence just before must not
        // then block for ever opening the FIFO to write.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        create(&path, version(1)).unwrap();
        let current = read(&path).unwrap().unwrap();
        std::fs::remove_file(&path).unwrap();
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        let target = path.clone();
        std::thread::spawn(move || {
            let _ = done.send(advance(&target, &current, version(2)).is_err());
        });
        let failed = finished.recv_timeout(std::time::Duration::from_secs(5));
        if failed.is_err() {
            // Free the blocked writer so the test process can exit.
            let _ = std::fs::File::open(&path);
        }
        assert_eq!(failed, Ok(true));
    }
}
