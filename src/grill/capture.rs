//! Splitting captured workload output into lines with stable positions.
//!
//! A runtime that captures a stream to an append-only file reads it in
//! chunks and feeds them to a [`CaptureReader`], which hands back whole
//! lines. Each line carries the byte offset just past its newline, so the log
//! store can tell a line it already ingested from a new one after the agent
//! restarts and reads the file again from the start.

use std::path::PathBuf;

use crate::ketchup::types::{CapturePosition, CapturedLine, LogStream};

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
        }
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
    pub fn push(&mut self, bytes: &[u8]) -> Vec<CapturedLine> {
        self.partial.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(newline) = self.partial.iter().position(|byte| *byte == b'\n') {
            let raw: Vec<u8> = self.partial.drain(..=newline).collect();
            self.consumed += raw.len() as u64;
            lines.push(self.line(&raw[..raw.len() - 1]));
        }
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
            }),
        }
    }
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

    #[test]
    fn in_memory_capture_has_no_position() {
        let mut reader = CaptureReader::new(LogStream::Stderr, None);
        let lines = reader.push(b"oops\n");
        assert_eq!(lines[0].stream, LogStream::Stderr);
        assert!(lines[0].position.is_none());
    }
}
