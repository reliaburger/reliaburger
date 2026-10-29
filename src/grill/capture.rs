//! Splitting captured workload output into lines with stable positions.
//!
//! A runtime that captures a stream to an append-only file reads it in
//! chunks and feeds them to a [`CaptureReader`], which hands back whole
//! lines. Each line carries the byte offset just past its newline, so the log
//! store can tell a line it already ingested from a new one. After an agent
//! restart a reader resumes at the offset the store checkpointed, rather
//! than reading the whole file again.

use std::io;
use std::path::{Path, PathBuf};

use crate::ketchup::types::{
    CaptureFileIdentity, CaptureOffsets, CapturePosition, CapturedLine, LogStream,
};

/// The most a follower reads from a capture file in one step.
///
/// A follower that starts from byte 0 (a capture file the log store has no
/// checkpoint for, or one that was truncated) may face hours of output. Reading and splitting it in
/// bounded chunks, with an await between them, keeps that replay from
/// holding a runtime worker: on a two-vCPU node one long synchronous split
/// starved startup adoption for eleven minutes.
pub const CAPTURE_CHUNK_BYTES: usize = 64 * 1024;

/// Read at most [`CAPTURE_CHUNK_BYTES`] of `path`, starting at `offset`.
///
/// A missing file, or an offset at or past its end, reads as empty. A chunk
/// of exactly [`CAPTURE_CHUNK_BYTES`] means more may be waiting.
pub async fn read_capture_chunk(path: &Path, offset: u64) -> io::Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    file.seek(io::SeekFrom::Start(offset)).await?;
    let mut chunk = Vec::with_capacity(CAPTURE_CHUNK_BYTES);
    file.take(CAPTURE_CHUNK_BYTES as u64)
        .read_to_end(&mut chunk)
        .await?;
    Ok(chunk)
}

/// How much of a capture file's end stays on disk after its ingested prefix
/// is reclaimed: the window a tail snapshot reads (#1418).
pub const RETAINED_CAPTURE_BYTES: u64 = 1024 * 1024;

/// Free the disk blocks holding the start of a capture file the log store
/// has already ingested, keeping the file's length and identity (#1418).
///
/// A capture file is append-only and the workload holds it open for as long
/// as it runs, so it can't be rotated without losing lines or moving every
/// offset the store has checkpointed. Punching a hole instead hands the
/// blocks back to the filesystem while the file keeps its length: offsets
/// still name the same lines, and the writer's `O_APPEND` writes carry on at
/// the end. The last [`RETAINED_CAPTURE_BYTES`] stay for tail snapshots, and
/// nothing past `ingested_through` is ever touched.
///
/// Returns where the reclaimed prefix ends; everything before it reads as
/// zeros. Nothing is freed when `path` has gone or is no longer the file
/// `identity` names. A filesystem that can't punch holes reports
/// [`io::ErrorKind::Unsupported`].
pub fn reclaim_ingested(
    path: &Path,
    identity: CaptureFileIdentity,
    ingested_through: u64,
) -> io::Result<u64> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = match std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if CaptureFileIdentity::of(&metadata) != identity {
        return Ok(0);
    }
    let block = metadata.blksize().max(1);
    let end = ingested_through.min(metadata.len().saturating_sub(RETAINED_CAPTURE_BYTES));
    let end = end - end % block;
    let start = data_start(&file, 0)?;
    let start = start - start % block;
    if start < end {
        punch_hole(&file, start, end - start)?;
    }
    Ok(end.max(start))
}

/// Where the first byte of data at or after `offset` lives: past any hole
/// [`reclaim_ingested`] punched. A file with no data there (or a filesystem
/// that can't say) answers `offset`.
fn data_start(file: &impl std::os::fd::AsFd, offset: u64) -> io::Result<u64> {
    use nix::unistd::{Whence, lseek};
    let Ok(signed) = i64::try_from(offset) else {
        return Ok(offset);
    };
    match lseek(file, signed, Whence::SeekData) {
        Ok(found) => Ok(u64::try_from(found).unwrap_or(offset)),
        // ENXIO: no data at or after `offset`, which is past the end.
        Err(nix::errno::Errno::ENXIO | nix::errno::Errno::EINVAL) => Ok(offset),
        Err(errno) => Err(errno.into()),
    }
}

#[cfg(target_os = "linux")]
fn punch_hole(file: &std::fs::File, offset: u64, length: u64) -> io::Result<()> {
    use nix::fcntl::{FallocateFlags, fallocate};
    let (Ok(offset), Ok(length)) = (i64::try_from(offset), i64::try_from(length)) else {
        return Ok(());
    };
    fallocate(
        file,
        FallocateFlags::FALLOC_FL_PUNCH_HOLE | FallocateFlags::FALLOC_FL_KEEP_SIZE,
        offset,
        length,
    )
    .map_err(|errno| match errno {
        nix::errno::Errno::EOPNOTSUPP => io::Error::from(io::ErrorKind::Unsupported),
        errno => errno.into(),
    })
}

#[cfg(target_os = "macos")]
fn punch_hole(file: &std::fs::File, offset: u64, length: u64) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let (Ok(offset), Ok(length)) = (i64::try_from(offset), i64::try_from(length)) else {
        return Ok(());
    };
    let hole = nix::libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: offset,
        fp_length: length,
    };
    // SAFETY: `file` is an open descriptor for the call's duration, and
    // F_PUNCHHOLE reads one `fpunchhole_t`, which `hole` is and outlives the
    // call. The kernel does not keep the pointer.
    let result = unsafe { nix::libc::fcntl(file.as_raw_fd(), nix::libc::F_PUNCHHOLE, &hole) };
    if result == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(nix::libc::ENOTSUP) {
            return Err(io::ErrorKind::Unsupported.into());
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn punch_hole(_file: &std::fs::File, _offset: u64, _length: u64) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Read a whole capture file, minus any prefix [`reclaim_ingested`] freed.
///
/// The line the reclaimed prefix ends in may have lost its start, so the
/// answer begins after its newline. The store already holds every line
/// in that range.
pub fn read_retained(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::{Read, Seek};
    let mut file = std::fs::File::open(path)?;
    let start = data_start(&file, 0)?;
    file.seek(io::SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if start > 0 {
        let cut = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |newline| newline + 1);
        bytes.drain(..cut);
    }
    Ok(bytes)
}

/// A bounded tail and the exact complete-line positions from the same snapshot.
/// Resume after these positions so a live follower never duplicates the tail.
#[derive(Default)]
pub(crate) struct TailSnapshot {
    pub text: String,
    pub offsets: CaptureOffsets,
}

impl TailSnapshot {
    pub fn add(
        &mut self,
        path: PathBuf,
        bytes: &[u8],
        start: u64,
        identity: Option<CaptureFileIdentity>,
    ) {
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1);
        let first = if start == 0 {
            0
        } else {
            bytes
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(complete, |at| at + 1)
        };
        self.text
            .push_str(&String::from_utf8_lossy(&bytes[first..complete]));
        self.offsets.0.insert(path.clone(), start + complete as u64);
        if let Some(identity) = identity {
            self.offsets.1.insert(path, identity);
        }
    }
    pub async fn files(stem: &Path) -> Self {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let mut snapshot = Self::default();
        for suffix in ["stdout", "stderr"] {
            let path = stem.with_extension(suffix);
            let Ok(mut file) = tokio::fs::File::open(&path).await else {
                continue;
            };
            let Ok(meta) = file.metadata().await else {
                continue;
            };
            let start = meta.len().saturating_sub(RETAINED_CAPTURE_BYTES);
            if file.seek(io::SeekFrom::Start(start)).await.is_err() {
                continue;
            }
            let mut bytes = Vec::new();
            if file
                .take(meta.len() - start)
                .read_to_end(&mut bytes)
                .await
                .is_ok()
            {
                snapshot.add(path, &bytes, start, Some(CaptureFileIdentity::of(&meta)));
            }
        }
        snapshot
    }
}

/// Turns chunks of one captured stream into [`CapturedLine`]s.
#[derive(Debug)]
pub struct CaptureReader {
    stream: LogStream,
    /// The capture file, when the stream is file-backed.
    file: Option<PathBuf>,
    /// Bytes handed back as complete lines so far.
    consumed: u64,
    /// Bytes read past the last newline, waiting for the rest of their line.
    partial: Vec<u8>,
    /// Dropping bytes up to the next newline: the read skipped a reclaimed
    /// prefix, so the line in progress lost its start.
    discarding: bool,
    identity: Option<CaptureFileIdentity>,
    #[cfg(test)]
    read_test_gate: Option<std::sync::Arc<CaptureReadGate>>,
}

impl CaptureReader {
    /// A reader for `stream`, positioned at the start of `file` (or of an
    /// in-memory buffer when `file` is `None`).
    pub fn new(stream: LogStream, file: Option<PathBuf>) -> Self {
        Self {
            stream,
            file,
            consumed: 0,
            partial: Vec::new(),
            discarding: false,
            identity: None,
            #[cfg(test)]
            read_test_gate: None,
        }
    }

    /// Resume an in-memory stream at a snapshot position. No file checkpoint
    /// or persistent identity is implied by this process-local byte offset.
    pub(crate) fn memory_at(stream: LogStream, offset: u64) -> Self {
        let mut reader = Self::new(stream, None);
        reader.consumed = offset;
        reader
    }

    /// A reader for the capture `file` of `stream`, positioned where the log
    /// store's checkpoint says it stopped.
    ///
    /// Falls back to byte 0 when the store holds nothing from `file`, or when
    /// the file is now shorter than the checkpointed offset: it was truncated
    /// (or replaced), so the offset no longer names a line in it. Resume also
    /// checks the offset's identity, and read_chunk checks the opened descriptor
    /// again so replacement after checkpoint loading cannot inherit that offset.
    pub async fn resume(stream: LogStream, file: PathBuf, offsets: &CaptureOffsets) -> Self {
        let metadata = tokio::fs::metadata(&file).await.ok();
        let identity = metadata.as_ref().map(CaptureFileIdentity::of);
        let consumed = offsets
            .get(&file)
            .filter(|offset| {
                metadata.as_ref().is_some_and(|meta| *offset <= meta.len())
                    && identity.is_some()
                    && offsets.identity(&file) == identity
            })
            .unwrap_or(0);
        Self {
            stream,
            file: Some(file),
            consumed,
            partial: Vec::new(),
            discarding: false,
            identity,
            #[cfg(test)]
            read_test_gate: None,
        }
    }

    /// Read bounded bytes and their identity from one opened descriptor.
    ///
    /// No reader state changes until every await has completed. Cancellation
    /// cannot discard a partial line or bind it to an unrelated replacement.
    pub async fn read_chunk(&mut self) -> io::Result<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let Some(path) = self.file.as_ref() else {
            return Ok(Vec::new());
        };
        let mut file = match tokio::fs::File::open(path).await {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata().await?;
        let identity = CaptureFileIdentity::of(&metadata);
        let replaced = self.identity.is_some_and(|old| old != identity);
        let reset = replaced || metadata.len() < self.read_offset();
        let offset = if reset { 0 } else { self.read_offset() };
        #[cfg(test)]
        if let Some(gate) = &self.read_test_gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        // A reader that starts inside a reclaimed prefix (a store that lost
        // its checkpoint reads from byte 0) jumps to the retained bytes
        // instead of handing on zeros (#1418).
        let start = data_start(&file, offset)?;
        file.seek(io::SeekFrom::Start(start)).await?;
        let mut chunk = Vec::with_capacity(CAPTURE_CHUNK_BYTES);
        file.take(CAPTURE_CHUNK_BYTES as u64)
            .read_to_end(&mut chunk)
            .await?;
        if reset {
            self.consumed = 0;
            self.partial.clear();
            self.discarding = false;
        }
        if start > offset {
            self.consumed = start;
            self.partial.clear();
            self.discarding = true;
        }
        self.identity = Some(identity);
        Ok(chunk)
    }

    /// The capture file this reader follows, if any.
    pub fn file(&self) -> Option<&std::path::Path> {
        self.file.as_deref()
    }

    /// Where the next chunk starts: everything already fed in.
    pub fn read_offset(&self) -> u64 {
        self.consumed + self.partial.len() as u64
    }

    /// Feed the next chunk and take the lines it completes.
    ///
    /// Offsets count raw bytes, so a line with invalid UTF-8 (replaced when
    /// the line becomes a `String`) doesn't shift the positions after it.
    ///
    /// The cost is linear in the bytes fed in: bytes already waiting hold no
    /// newline, so only the new ones are searched, and the finished lines
    /// leave the buffer in one step at the end.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<CapturedLine> {
        let bytes = if self.discarding {
            let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') else {
                self.consumed += bytes.len() as u64;
                return Vec::new();
            };
            self.consumed += newline as u64 + 1;
            self.discarding = false;
            &bytes[newline + 1..]
        } else {
            bytes
        };
        let mut search_from = self.partial.len();
        self.partial.extend_from_slice(bytes);
        let mut lines = Vec::new();
        let mut line_start = 0;
        while let Some(found) = self.partial[search_from..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let newline = search_from + found;
            self.consumed += (newline + 1 - line_start) as u64;
            lines.push(self.line(&self.partial[line_start..newline]));
            line_start = newline + 1;
            search_from = line_start;
        }
        self.partial.drain(..line_start);
        lines
    }

    /// Take a trailing line that never got its newline, once the stream has
    /// ended.
    pub fn finish(&mut self) -> Option<CapturedLine> {
        if self.partial.is_empty() {
            return None;
        }
        let raw = std::mem::take(&mut self.partial);
        self.consumed += raw.len() as u64;
        Some(self.line(&raw))
    }

    fn line(&self, raw: &[u8]) -> CapturedLine {
        CapturedLine {
            stream: self.stream,
            line: String::from_utf8_lossy(raw).into_owned(),
            position: self.file.as_ref().map(|file| CapturePosition {
                file: file.clone(),
                end_offset: self.consumed,
                identity: self.identity,
            }),
        }
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
struct CaptureReadGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_reader() -> CaptureReader {
        CaptureReader::new(LogStream::Stdout, Some(PathBuf::from("/logs/web.stdout")))
    }

    fn ends(lines: &[CapturedLine]) -> Vec<u64> {
        lines
            .iter()
            .map(|line| line.position.as_ref().unwrap().end_offset)
            .collect()
    }

    #[test]
    fn each_line_ends_just_past_its_newline() {
        let mut reader = file_reader();
        let lines = reader.push(b"ACK 1\nACK 2\n");
        assert_eq!(lines[0].line, "ACK 1");
        assert_eq!(lines[1].line, "ACK 2");
        assert_eq!(ends(&lines), vec![6, 12]);
        assert_eq!(lines[0].stream, LogStream::Stdout);
    }

    #[test]
    fn a_line_split_across_chunks_arrives_once_whole() {
        let mut reader = file_reader();
        assert!(reader.push(b"ACK 1").is_empty());
        assert_eq!(reader.read_offset(), 5);
        let lines = reader.push(b"0\nACK");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line, "ACK 10");
        assert_eq!(ends(&lines), vec![7]);
        assert_eq!(reader.read_offset(), 10);
    }

    #[test]
    fn invalid_utf8_does_not_shift_later_offsets() {
        let mut reader = file_reader();
        let lines = reader.push(b"\xff\xfe\nok\n");
        assert_eq!(lines[0].line, "\u{fffd}\u{fffd}");
        assert_eq!(ends(&lines), vec![3, 6]);
    }

    #[test]
    fn finish_returns_an_unterminated_last_line() {
        let mut reader = file_reader();
        reader.push(b"done\nhalf");
        let last = reader.finish().unwrap();
        assert_eq!(last.line, "half");
        assert_eq!(last.position.unwrap().end_offset, 9);
        assert!(reader.finish().is_none());
    }

    /// Splitting must cost time in proportion to the bytes fed in. The first
    /// version drained each line from the front of its buffer, moving the
    /// rest every time: a 56 MB replay of short lines took minutes of CPU.
    #[test]
    fn a_backlog_of_short_lines_splits_in_linear_time() {
        const LINES: usize = 512 * 1024;
        let backlog = "abcdefg\n".repeat(LINES);
        let mut reader = file_reader();
        let started = std::time::Instant::now();
        let lines = reader.push(backlog.as_bytes());
        let elapsed = started.elapsed();
        assert_eq!(lines.len(), LINES);
        assert_eq!(ends(&lines[LINES - 1..]), vec![backlog.len() as u64]);
        assert_eq!(reader.read_offset(), backlog.len() as u64);
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "splitting {LINES} lines took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn capture_chunks_tail_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("output.log");

        // Missing file reads as empty, not an error.
        assert!(read_capture_chunk(&path, 0).await.unwrap().is_empty());

        std::fs::write(&path, b"line one\n").unwrap();
        let first = read_capture_chunk(&path, 0).await.unwrap();
        assert_eq!(first, b"line one\n");

        // Reading from the end yields nothing until more is appended.
        let offset = first.len() as u64;
        assert!(read_capture_chunk(&path, offset).await.unwrap().is_empty());

        std::fs::write(&path, b"line one\nline two\n").unwrap();
        let second = read_capture_chunk(&path, offset).await.unwrap();
        assert_eq!(second, b"line two\n");
    }

    #[tokio::test]
    async fn a_long_capture_is_read_in_bounded_chunks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("output.log");
        let contents: Vec<u8> = (0..CAPTURE_CHUNK_BYTES * 2 + 10)
            .map(|index| (index % 251) as u8)
            .collect();
        std::fs::write(&path, &contents).unwrap();

        let mut read = Vec::new();
        let mut sizes = Vec::new();
        loop {
            let chunk = read_capture_chunk(&path, read.len() as u64).await.unwrap();
            if chunk.is_empty() {
                break;
            }
            sizes.push(chunk.len());
            read.extend(chunk);
        }
        assert_eq!(sizes, vec![CAPTURE_CHUNK_BYTES, CAPTURE_CHUNK_BYTES, 10]);
        assert_eq!(read, contents);
    }

    /// #308: a restarted forwarder starts where the store's checkpoint
    /// stopped, so the lines before it are never read again.
    #[tokio::test]
    async fn resume_starts_at_the_checkpointed_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("web.stdout");
        std::fs::write(&path, b"one\ntwo\nthree\n").unwrap();
        let offsets = CaptureOffsets(
            [(path.clone(), 8)].into_iter().collect(),
            [(
                path.clone(),
                CaptureFileIdentity::of(&std::fs::metadata(&path).unwrap()),
            )]
            .into_iter()
            .collect(),
        );

        let mut reader = CaptureReader::resume(LogStream::Stdout, path.clone(), &offsets).await;
        assert_eq!(reader.read_offset(), 8);
        let chunk = read_capture_chunk(&path, reader.read_offset())
            .await
            .unwrap();
        let lines = reader.push(&chunk);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line, "three");
        assert_eq!(ends(&lines), vec![14]);
    }

    #[tokio::test]
    async fn resume_without_a_checkpoint_starts_at_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("web.stdout");
        std::fs::write(&path, b"one\n").unwrap();
        let reader =
            CaptureReader::resume(LogStream::Stdout, path, &CaptureOffsets::default()).await;
        assert_eq!(reader.read_offset(), 0);
    }

    /// A capture file shorter than its checkpoint was truncated: the offset
    /// no longer names a line in it, so everything in it is new.
    #[tokio::test]
    async fn resume_falls_back_to_zero_when_the_file_was_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("web.stdout");
        std::fs::write(&path, b"new\n").unwrap();
        let offsets = CaptureOffsets(
            [(path.clone(), 14)].into_iter().collect(),
            [(
                path.clone(),
                CaptureFileIdentity::of(&std::fs::metadata(&path).unwrap()),
            )]
            .into_iter()
            .collect(),
        );
        let reader = CaptureReader::resume(LogStream::Stdout, path.clone(), &offsets).await;
        assert_eq!(reader.read_offset(), 0);

        std::fs::remove_file(&path).unwrap();
        let reader = CaptureReader::resume(LogStream::Stdout, path, &offsets).await;
        assert_eq!(reader.read_offset(), 0);
    }

    #[test]
    fn in_memory_capture_has_no_position() {
        let mut reader = CaptureReader::new(LogStream::Stderr, None);
        let lines = reader.push(b"oops\n");
        assert_eq!(lines[0].stream, LogStream::Stderr);
        assert!(lines[0].position.is_none());
    }

    #[tokio::test]
    async fn capture_identity_is_observed_on_the_descriptor_that_supplied_the_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("writer.stdout");
        std::fs::write(&file, b"old-line\n").unwrap();
        let held_old_inode = std::fs::File::open(&file).unwrap();
        let old_identity = CaptureFileIdentity::of(&held_old_inode.metadata().unwrap());
        let mut reader =
            CaptureReader::resume(LogStream::Stdout, file.clone(), &CaptureOffsets::default())
                .await;
        let gate = std::sync::Arc::new(CaptureReadGate::default());
        reader.read_test_gate = Some(gate.clone());
        let read = tokio::spawn(async move {
            let bytes = reader.read_chunk().await.unwrap();
            reader.push(&bytes)
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), gate.entered.notified())
            .await
            .unwrap();
        let replacement = directory.path().join("replacement.stdout");
        std::fs::write(&replacement, b"new-line\n").unwrap();
        std::fs::rename(replacement, &file).unwrap();
        gate.release.notify_one();
        let lines = tokio::time::timeout(std::time::Duration::from_secs(10), read)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(lines[0].line, "old-line");
        assert_eq!(
            lines[0].position.as_ref().unwrap().identity,
            Some(old_identity)
        );
        assert_ne!(
            old_identity,
            CaptureFileIdentity::of(&std::fs::metadata(file).unwrap())
        );
    }

    #[tokio::test]
    async fn canceling_a_capture_read_preserves_partial_bytes_and_identity_until_retry() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("writer.stdout");
        std::fs::write(&file, b"one\npartial").unwrap();
        let held_old_inode = std::fs::File::open(&file).unwrap();
        let mut reader =
            CaptureReader::resume(LogStream::Stdout, file.clone(), &CaptureOffsets::default())
                .await;
        let first = reader.read_chunk().await.unwrap();
        assert_eq!(reader.push(&first)[0].line, "one");
        let original_identity = reader.identity;
        let original_partial = reader.partial.clone();
        let original_offset = reader.read_offset();
        let reader = std::sync::Arc::new(tokio::sync::Mutex::new(reader));
        let gate = std::sync::Arc::new(CaptureReadGate::default());
        reader.lock().await.read_test_gate = Some(gate.clone());
        let replacement = directory.path().join("replacement.stdout");
        std::fs::write(&replacement, b"new-line\n").unwrap();
        std::fs::rename(replacement, &file).unwrap();
        let owned_reader = reader.clone();
        let reading = tokio::spawn(async move { owned_reader.lock().await.read_chunk().await });
        tokio::time::timeout(std::time::Duration::from_secs(10), gate.entered.notified())
            .await
            .unwrap();
        reading.abort();
        assert!(reading.await.unwrap_err().is_cancelled());
        let mut reader = reader.lock().await;
        assert_eq!(reader.identity, original_identity);
        assert_eq!(reader.partial, original_partial);
        assert_eq!(reader.read_offset(), original_offset);
        reader.read_test_gate = None;
        let bytes = reader.read_chunk().await.unwrap();
        let lines = reader.push(&bytes);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line, "new-line");
        assert_eq!(lines[0].position.as_ref().unwrap().end_offset, 9);
        assert_eq!(
            lines[0].position.as_ref().unwrap().identity,
            Some(CaptureFileIdentity::of(&std::fs::metadata(&file).unwrap()))
        );
        drop(held_old_inode);
    }

    /// A capture of `lines` numbered lines, each 32 bytes with its newline.
    fn numbered_capture(path: &Path, lines: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(lines * 32);
        for index in 0..lines {
            bytes.extend_from_slice(format!("spam {index:09} quick brown fox.\n").as_bytes());
        }
        std::fs::write(path, &bytes).unwrap();
        bytes
    }

    fn allocated_bytes(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).unwrap().blocks() * 512
    }

    fn identity_of(path: &Path) -> CaptureFileIdentity {
        CaptureFileIdentity::of(&std::fs::metadata(path).unwrap())
    }

    /// #1418: a chatty app's capture file grew by its whole output for as
    /// long as it ran. Once the store holds the lines, their blocks go back
    /// to the filesystem, while the length (and so every offset) stays.
    #[test]
    fn reclaiming_frees_ingested_bytes_but_keeps_the_length_and_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("spammer.stdout");
        let contents = numbered_capture(&path, 256 * 1024);
        let identity = identity_of(&path);
        assert!(allocated_bytes(&path) >= contents.len() as u64);

        let reclaimed = reclaim_ingested(&path, identity, contents.len() as u64).unwrap();

        assert!(reclaimed > 0, "nothing was reclaimed");
        assert!(reclaimed <= contents.len() as u64 - RETAINED_CAPTURE_BYTES);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            contents.len() as u64
        );
        assert_eq!(identity_of(&path), identity);
        assert!(
            allocated_bytes(&path) <= RETAINED_CAPTURE_BYTES + 256 * 1024,
            "{} bytes still allocated",
            allocated_bytes(&path)
        );
        // Reclaiming again is a no-op that reports the same boundary.
        assert_eq!(
            reclaim_ingested(&path, identity, contents.len() as u64).unwrap(),
            reclaimed
        );
    }

    #[test]
    fn reclaiming_never_frees_bytes_the_store_has_not_ingested() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("spammer.stdout");
        let contents = numbered_capture(&path, 256 * 1024);
        let identity = identity_of(&path);
        let ingested = 2 * 1024 * 1024;

        let reclaimed = reclaim_ingested(&path, identity, ingested).unwrap();

        assert!(reclaimed <= ingested);
        let mut file = std::fs::File::open(&path).unwrap();
        let mut rest = Vec::new();
        use std::io::{Read, Seek};
        file.seek(io::SeekFrom::Start(ingested)).unwrap();
        file.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, contents[ingested as usize..]);
    }

    #[test]
    fn reclaiming_a_replaced_file_frees_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("spammer.stdout");
        numbered_capture(&path, 256 * 1024);
        let old = identity_of(&path);
        let replacement = tmp.path().join("replacement");
        let contents = numbered_capture(&replacement, 256 * 1024);
        std::fs::rename(&replacement, &path).unwrap();

        assert_eq!(
            reclaim_ingested(&path, old, contents.len() as u64).unwrap(),
            0
        );
        assert_eq!(std::fs::read(&path).unwrap(), contents);
        // A file that has gone is not an error either.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(reclaim_ingested(&path, old, 1 << 30).unwrap(), 0);
    }

    /// A tail snapshot reads the last mebibyte, which reclaiming keeps.
    #[tokio::test]
    async fn a_tail_after_reclaiming_is_whole_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let stem = tmp.path().join("spammer");
        let path = stem.with_extension("stdout");
        let contents = numbered_capture(&path, 256 * 1024);
        reclaim_ingested(&path, identity_of(&path), contents.len() as u64).unwrap();

        let snapshot = TailSnapshot::files(&stem).await;

        assert!(!snapshot.text.contains('\0'));
        let last = snapshot.text.lines().last().unwrap();
        assert_eq!(last, "spam 000262143 quick brown fox.");
        assert_eq!(snapshot.offsets.0[&path], contents.len() as u64);
    }

    /// A store that lost its checkpoint reads the file from byte 0 again. The
    /// reclaimed prefix is skipped, not handed on as one line of zeros, and
    /// the line it cut in half is dropped rather than stored as a fragment.
    #[tokio::test]
    async fn a_reader_from_byte_zero_skips_a_reclaimed_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("spammer.stdout");
        let contents = numbered_capture(&path, 256 * 1024);
        let reclaimed = reclaim_ingested(&path, identity_of(&path), contents.len() as u64).unwrap();
        assert!(reclaimed > 0);

        let mut reader = CaptureReader::new(LogStream::Stdout, Some(path.clone()));
        let mut lines = Vec::new();
        loop {
            let chunk = reader.read_chunk().await.unwrap();
            if chunk.is_empty() {
                break;
            }
            lines.extend(reader.push(&chunk));
        }

        let first = &lines[0];
        let end = first.position.as_ref().unwrap().end_offset;
        // The line the prefix ends in may have lost its start, so it is
        // dropped too: the first line read is the one after it.
        assert!(
            end > reclaimed && end - reclaimed <= 64,
            "first line ends at {end}"
        );
        assert_eq!(end % 32, 0, "the first line is a whole line");
        assert_eq!(
            first.line,
            format!("spam {:09} quick brown fox.", end / 32 - 1)
        );
        assert!(lines.iter().all(|line| !line.line.contains('\0')));
        assert_eq!(lines.len() as u64, (contents.len() as u64 - end) / 32 + 1);
        assert_eq!(reader.read_offset(), contents.len() as u64);
    }

    #[test]
    fn read_retained_skips_the_reclaimed_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("spammer.stdout");
        let contents = numbered_capture(&path, 256 * 1024);
        let reclaimed = reclaim_ingested(&path, identity_of(&path), contents.len() as u64).unwrap();

        let retained = read_retained(&path).unwrap();

        assert!(!retained.contains(&0));
        // Everything after the first newline past the reclaimed prefix: the
        // line the prefix ends in may have lost its start.
        let cut = reclaimed as usize;
        let start = cut
            + contents[cut..]
                .iter()
                .position(|byte| *byte == b'\n')
                .unwrap()
            + 1;
        assert_eq!(retained, contents[start..]);
        // A capture nothing was reclaimed from reads whole.
        let small = tmp.path().join("small.stdout");
        std::fs::write(&small, b"one\ntwo\n").unwrap();
        assert_eq!(read_retained(&small).unwrap(), b"one\ntwo\n");
    }
}
