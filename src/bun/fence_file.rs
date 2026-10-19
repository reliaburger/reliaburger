//! A small file that a task-array node rewrites in place, durably, with one
//! flush: the shape of both its control fence and its per-array grant
//! fences.
//!
//! A fence changes often under load. Replacing a file (write a temporary,
//! flush it, rename, flush the directory) costs two flushes plus a create
//! and a rename, and on macOS each of those queues behind every other
//! process's `F_FULLFSYNC` (#745, #902). So the file has two slots, each a
//! generation number, a payload and a CRC32, and a write overwrites the
//! slot that doesn't hold the current generation, in place, with one
//! flush. The other slot keeps the last payload the node made durable, so
//! a crash that tears the write loses only the payload it was writing,
//! which the node never acted on.
//!
//! Each slot has a fixed capacity, the file's stride. A payload that
//! outgrows it moves the fence to a new file with twice the room, written
//! the safe way (temporary, flush, rename, directory flush), so that costs
//! two flushes once per doubling.

use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Where the newest payload sits in a fence file, so the next write knows
/// which slot to overwrite and how much room it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Position {
    /// Counts the writes; the newer of two valid slots has the higher one.
    generation: u64,
    /// The slot holding the newest payload, 0 or 1.
    slot: u64,
    /// The distance between the slots, and so each slot's capacity.
    stride: u64,
}

/// A fence file as read from disk.
#[derive(Debug, Clone)]
pub(crate) struct Contents {
    /// The newest payload the node made durable.
    pub payload: Vec<u8>,
    /// Where it sits.
    pub position: Position,
}

/// What one write did.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Written {
    /// Where the payload now sits.
    pub position: Position,
    /// How many flushes it cost: one in place, two for a new file.
    pub flushes: u64,
}

/// Magic, generation and payload length, before the payload.
const HEADER_LEN: usize = 4 + 8 + 4;
/// The CRC32 of the header and payload, after the payload.
const CHECK_LEN: usize = 4;
/// The smallest stride. Slots sit in separate 512-byte sectors, so a torn
/// write of one can't reach the other, and a small fence stays small (a
/// test bounds what a node's task directory keeps after its runs retire).
const MIN_STRIDE: u64 = 512;

fn invalid(path: &Path, what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("fence {} {what}", path.display()),
    )
}

/// One slot's bytes: magic, generation, payload length, payload, CRC32.
fn encode(magic: &[u8; 4], generation: u64, payload: &[u8]) -> io::Result<Vec<u8>> {
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "fence payload too large"))?;
    let mut slot = Vec::with_capacity(HEADER_LEN + payload.len() + CHECK_LEN);
    slot.extend_from_slice(magic);
    slot.extend_from_slice(&generation.to_le_bytes());
    slot.extend_from_slice(&length.to_le_bytes());
    slot.extend_from_slice(payload);
    let crc = crc32fast::hash(&slot);
    slot.extend_from_slice(&crc.to_le_bytes());
    Ok(slot)
}

/// The generation and payload in one slot, or `None` if the slot is torn,
/// never written, or of another kind.
fn decode<'a>(magic: &[u8; 4], slot: &'a [u8]) -> Option<(u64, &'a [u8])> {
    let generation = u64::from_le_bytes(slot.get(4..12)?.try_into().ok()?);
    let length = u32::from_le_bytes(slot.get(12..HEADER_LEN)?.try_into().ok()?);
    let end = HEADER_LEN.checked_add(usize::try_from(length).ok()?)?;
    let crc = u32::from_le_bytes(slot.get(end..end + CHECK_LEN)?.try_into().ok()?);
    if slot.get(..4)? != magic || crc32fast::hash(slot.get(..end)?) != crc {
        return None;
    }
    Some((generation, slot.get(HEADER_LEN..end)?))
}

/// Read the fence at `path`. `None` when there is no file yet; an error
/// when the file exists but neither slot holds a payload, because reading
/// that as no fence would let through what the fence exists to refuse.
pub(crate) fn read(path: &Path, magic: &[u8; 4]) -> io::Result<Option<Contents>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // Every file this module writes is two slots of a power-of-two stride.
    let stride = bytes.len() / 2;
    if bytes.len() % 2 != 0 || !stride.is_power_of_two() || (stride as u64) < MIN_STRIDE {
        return Err(invalid(path, &format!("is {} bytes", bytes.len())));
    }
    let (first, second) = bytes.split_at(stride);
    let newest = match (decode(magic, first), decode(magic, second)) {
        (Some(a), Some(b)) if b.0 > a.0 => (1, b),
        (Some(a), _) => (0, a),
        (None, Some(b)) => (1, b),
        (None, None) => return Err(invalid(path, "has no valid slot")),
    };
    let (slot, (generation, payload)) = newest;
    Ok(Some(Contents {
        payload: payload.to_vec(),
        position: Position {
            generation,
            slot,
            stride: stride as u64,
        },
    }))
}

/// Make `payload` the fence at `path`, durably. With `current`, the
/// position [`read`] or the last write returned, it overwrites the other
/// slot in place with one flush when the payload fits; otherwise (no file
/// yet, or no room) it writes a whole new file beside `path`, flushes it,
/// renames it into place and flushes the directory.
pub(crate) fn write(
    path: &Path,
    magic: &[u8; 4],
    current: Option<&Position>,
    payload: &[u8],
) -> io::Result<Written> {
    let generation = match current {
        Some(current) => current
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid(path, "ran out of generations"))?,
        None => 1,
    };
    let slot = encode(magic, generation, payload)?;
    match current {
        Some(current) if slot.len() as u64 <= current.stride => {
            rewrite_slot(path, current, &slot)?;
            Ok(Written {
                position: Position {
                    generation,
                    slot: 1 - current.slot,
                    stride: current.stride,
                },
                flushes: 1,
            })
        }
        _ => create(path, generation, &slot),
    }
}

/// Overwrite the slot that doesn't hold `current`, in place, with one
/// flush.
fn rewrite_slot(path: &Path, current: &Position, slot: &[u8]) -> io::Result<()> {
    // O_NONBLOCK keeps an open of something that isn't a regular file (a
    // FIFO swapped in underneath) from waiting for ever; it changes nothing
    // for a regular file.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != 2 * current.stride {
        return Err(invalid(path, "changed underneath the node"));
    }
    file.seek(SeekFrom::Start((1 - current.slot) * current.stride))?;
    file.write_all(slot)?;
    // The file's size and directory entry are unchanged and already
    // durable, so its data is all that needs flushing. `sync_data` is
    // `fdatasync` on Linux and `F_FULLFSYNC` on macOS.
    file.sync_data()
}

/// Write a new fence file holding `slot` in both slots, with room for a
/// payload twice its size. The file appears whole or not at all: it is
/// written beside its final name, flushed, renamed into place, and then
/// its directory is flushed. Two flushes.
fn create(path: &Path, generation: u64, slot: &[u8]) -> io::Result<Written> {
    let stride = (2 * slot.len() as u64).next_power_of_two().max(MIN_STRIDE);
    let width = usize::try_from(stride).map_err(|_| invalid(path, "would be too large"))?;
    let mut bytes = vec![0u8; 2 * width];
    bytes[..slot.len()].copy_from_slice(slot);
    bytes[width..width + slot.len()].copy_from_slice(slot);
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid(path, "has no parent directory"))?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(Written {
        position: Position {
            generation,
            slot: 0,
            stride,
        },
        flushes: 2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    const MAGIC: &[u8; 4] = b"RBTF";

    /// Damage one byte of a slot's payload, as a write torn by a crash
    /// would.
    fn tear(path: &Path, slot: u64, stride: u64) {
        use std::io::Read;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let offset = slot * stride + 16;
        let mut byte = [0u8];
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.read_exact(&mut byte).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&[byte[0] ^ 0xff]).unwrap();
    }

    #[test]
    fn a_new_fence_reads_back_its_payload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.fence");
        assert!(read(&path, MAGIC).unwrap().is_none());
        let written = write(&path, MAGIC, None, b"seven").unwrap();
        // The file and its directory.
        assert_eq!(written.flushes, 2);
        let contents = read(&path, MAGIC).unwrap().unwrap();
        assert_eq!(contents.payload, b"seven");
        assert_eq!(contents.position, written.position);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1024);
    }

    #[test]
    fn writing_alternates_slots_in_place_so_the_previous_payload_survives() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.fence");
        let mut position = write(&path, MAGIC, None, b"1").unwrap().position;
        let inode = std::fs::metadata(&path).unwrap().ino();
        for payload in [b"2", b"3", b"4", b"5"] {
            let written = write(&path, MAGIC, Some(&position), payload).unwrap();
            assert_eq!(written.flushes, 1);
            assert_ne!(written.position.slot, position.slot);
            let contents = read(&path, MAGIC).unwrap().unwrap();
            assert_eq!(contents.payload, payload);
            assert_eq!(contents.position, written.position);
            position = written.position;
        }
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    }

    #[test]
    fn a_torn_slot_falls_back_to_the_last_flushed_payload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.fence");
        let first = write(&path, MAGIC, None, b"old").unwrap().position;
        let second = write(&path, MAGIC, Some(&first), b"new").unwrap().position;
        tear(&path, second.slot, second.stride);
        let contents = read(&path, MAGIC).unwrap().unwrap();
        assert_eq!(contents.payload, b"old");
        // The next write overwrites the torn slot, never the good one.
        let third = write(&path, MAGIC, Some(&contents.position), b"newer")
            .unwrap()
            .position;
        assert_eq!(third.slot, second.slot);
        tear(&path, third.slot, third.stride);
        assert_eq!(read(&path, MAGIC).unwrap().unwrap().payload, b"old");
    }

    #[test]
    fn a_fence_with_no_valid_slot_is_an_error_not_an_empty_fence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.fence");
        let position = write(&path, MAGIC, None, b"payload").unwrap().position;
        tear(&path, 0, position.stride);
        tear(&path, 1, position.stride);
        assert_eq!(
            read(&path, MAGIC).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_fence_of_another_kind_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.fence");
        write(&path, b"RBXX", None, b"payload").unwrap();
        assert_eq!(
            read(&path, MAGIC).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_fence_of_the_wrong_length_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.fence");
        std::fs::write(&path, b"{}").unwrap();
        assert_eq!(
            read(&path, MAGIC).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::write(&path, vec![0u8; 1536]).unwrap();
        assert_eq!(
            read(&path, MAGIC).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_payload_that_outgrows_its_slot_moves_to_a_larger_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.fence");
        let small = write(&path, MAGIC, None, &[1u8; 100]).unwrap().position;
        let large = vec![2u8; 600];
        let grown = write(&path, MAGIC, Some(&small), &large).unwrap();
        // A new file, renamed into place: the file and its directory.
        assert_eq!(grown.flushes, 2);
        let contents = read(&path, MAGIC).unwrap().unwrap();
        assert_eq!(contents.payload, large);
        assert!(std::fs::metadata(&path).unwrap().len() > 1024);
        // There is room again, so the next write is in place.
        let next = write(&path, MAGIC, Some(&grown.position), &[3u8; 610]).unwrap();
        assert_eq!(next.flushes, 1);
        assert_eq!(read(&path, MAGIC).unwrap().unwrap().payload, [3u8; 610]);
        // And a shrinking payload stays in the larger file.
        let shrunk = write(&path, MAGIC, Some(&next.position), b"tiny").unwrap();
        assert_eq!(shrunk.flushes, 1);
        assert_eq!(read(&path, MAGIC).unwrap().unwrap().payload, b"tiny");
    }

    #[test]
    fn writing_a_fence_swapped_for_a_fifo_fails_instead_of_blocking() {
        // A test hangs a worker by swapping its control fence for a FIFO
        // with no writer. A sync that read the real fence just before must
        // not then block for ever opening the FIFO to write.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.fence");
        let position = write(&path, MAGIC, None, b"1").unwrap().position;
        std::fs::remove_file(&path).unwrap();
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        let target = path.clone();
        std::thread::spawn(move || {
            let _ = done.send(write(&target, MAGIC, Some(&position), b"2").is_err());
        });
        let failed = finished.recv_timeout(std::time::Duration::from_secs(5));
        if failed.is_err() {
            // Free the blocked writer so the test process can exit.
            let _ = std::fs::File::open(&path);
        }
        assert_eq!(failed, Ok(true));
    }
}
