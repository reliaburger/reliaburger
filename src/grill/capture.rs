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
            identity: None,
            #[cfg(test)]
            read_test_gate: None,
        }
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
        file.seek(io::SeekFrom::Start(offset)).await?;
        let mut chunk = Vec::with_capacity(CAPTURE_CHUNK_BYTES);
        file.take(CAPTURE_CHUNK_BYTES as u64)
            .read_to_end(&mut chunk)
            .await?;
        if reset {
            self.consumed = 0;
            self.partial.clear();
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
}
